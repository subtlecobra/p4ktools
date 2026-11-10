//! Weapons: the index of every weapon item of Game2.dcb, their variants (skins) and the port / attachment rules of one weapon (the 3D model
//! assembly is in `weaponmodel`).
//!
//! What the archive holds (EMPIRICAL on the live archive):
//!
//! * A weapon is an `EntityClassDefinition` whose `SAttachableComponentParams.AttachDef.Type` is `WeaponPersonal` (first-person
//!   weapons: Small = pistols, Medium = rifles / SMGs / shotguns / snipers, Large = LMGs / launchers, Gadget = multitools ...; Knife =
//!   melee, no `SCItemWeaponComponentParams` but `SMeleeWeaponComponentParams`), `WeaponGun` / `WeaponMining` (ship guns, mining lasers),
//!   `Missile` / `Bomb` (ammunition), `MissileLauncher` / `BombLauncher` (racks) or `Turret` / `TurretBase` (mounts that carry guns).
//!   Grenades and proximity mines are not indexed.
//! * The item's kind is the line `Item Type: ...` of its localised description and its damage class the line `Class: ...`.
//! * First-person weapons have a `.cdf` model, ship weapons a `.cgf` / `.cga`.
//! * A weapon's ports are `SItemPortContainerComponentParams.Ports[]`.  FPS weapons: `magazine_attach`, `optics_attach`,
//!   `barrel_attach`, `underbarrel_attach`.  A port is offered when it lists item types and is not `uneditable` (INFERRED).
//! * Skins: a variant of a weapon is another entity class sharing its model whose name extends the base's by `_<suffix>`.  The base's
//!   `SGeometryResourceParams.Geometry.SubGeometry` list carries one node per skin, `Tags` = the suffix (any case), with an optional
//!   `Material` and a `Palette`; an empty field inherits the main node's (INFERRED).
//! * CONFIRMED against the engine: a skin is a MODEL TAG matched against the `Tags`
//!   of the `SubGeometry` nodes; attachments inherit the host tag (`inheritModelTagFromHost`) and use their own node's palette /
//!   material; `rootOverridePaint` makes a child take the host's.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use rayon::prelude::*;
use regex::Regex;
use serde_json::{json, Value};

use crate::dynv::*;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::{comp, ItemStats, RECORD};
use crate::weaponstats::{classify_damage, WeaponStats};

pub const GROUPS: [(&str, &str); 6] = [
    ("fps", "Personal weapons"),
    ("melee", "Melee"),
    ("ship_gun", "Ship guns"),
    ("missile", "Missiles and bombs"),
    ("rack", "Missile racks and launchers"),
    ("turret", "Turrets and mounts"),
];
const TYPE_GROUP: [(&str, &str); 11] = [
    ("WeaponPersonal", "fps"),
    ("WeaponGun", "ship_gun"),
    ("WeaponMining", "ship_gun"),
    ("Missile", "missile"),
    ("Bomb", "missile"),
    ("MissileLauncher", "rack"),
    ("BombLauncher", "rack"),
    ("GroundVehicleMissileLauncher", "rack"),
    ("Turret", "turret"),
    ("TurretBase", "turret"),
    ("UtilityTurret", "turret"),
];
pub const MODEL_EXT: [&str; 3] = [".cdf", ".cgf", ".cga"];
pub const MAX_DEPTH: usize = 3;
pub const ATTACH_LABELS: [(&str, &str); 9] = [
    ("magazine", "Magazine"),
    ("ironsight", "Optic"),
    ("barrel", "Barrel"),
    ("bottomattachment", "Underbarrel"),
    ("ventilation", "Ventilation"),
    ("firingmechanism", "Firing mechanism"),
    ("powerarray", "Power array"),
    ("missile", "Missile"),
    ("utility", "Utility"),
];

/// Group id of an item from its AttachDef.Type and its component names, or None when it is not a weapon item (knives are melee,
/// grenades and mines are left out, a WeaponPersonal item with neither weapon component is not listed).
pub fn group_of(att_type: &str, has: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    let g = TYPE_GROUP.iter().find(|(t, _)| *t == att_type).map(|x| x.1)?;
    if g == "fps" {
        if has("SCItemWeaponComponentParams") {
            return Some("fps");
        }
        return if has("SMeleeWeaponComponentParams") { Some("melee") } else { None };
    }
    Some(g)
}

/// '' for a localisation placeholder ('<= PLACEHOLDER =>'), else the text.
pub fn clean_text(t: &str) -> String {
    if t.is_empty() || t.contains("PLACEHOLDER") {
        String::new()
    } else {
        t.to_string()
    }
}

fn re_kind() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?m)^Item Type:\s*(.+)$").unwrap())
}
fn re_class() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?m)^Class:\s*(.+)$").unwrap())
}
fn re_skip() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(^|_)(template|test|dev|placeholder)(_|$)").unwrap())
}

/// Value of the `Item Type:` (`kind`) / `Class:` (`dclass`) line of a localised item description, or ''.
pub fn description_field(text: &str, key: &str) -> String {
    let re = if key == "kind" { re_kind() } else { re_class() };
    re.captures(text).map_or(String::new(), |c| c[1].trim().to_string())
}

/// The match groups above are stripped with `trim` (whitespace); fine for ASCII text.
fn in_archive(p4k: &p4k::P4k, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let ext = match path.rfind('.') {
        Some(i) => path[i..].to_lowercase(),
        None => path.chars().last().map(|c| c.to_lowercase().to_string()).unwrap_or_default(),
    };
    p4k.find_path(path, &ext).is_some()
}

/// Entity class record names in record order (the order of the record index), with the first record index of each.
pub fn ordered_names(items: &ItemMaterials, prefix: &str) -> Vec<(String, usize)> {
    let Some(d) = items.datacore() else { return vec![] };
    let mut v: Vec<(String, usize)> = d.record_index().iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, ids)| (k.clone(), ids[0])).collect();
    v.sort_by_key(|x| x.1);
    v
}

/// The models and skins side of the weapon index; caches `model_path`.
pub struct Weapons<'a> {
    pub items: &'a ItemMaterials,
    pub stats: &'a ItemStats<'a>,
    model_cache: Mutex<HashMap<String, Option<String>>>,
    pal_rev: OnceLock<HashMap<usize, String>>,
    ports_cache: Mutex<HashMap<String, std::sync::Arc<Vec<(String, Port)>>>>,
}

/// One item port definition (the part of `swaps::Port` the weapon code reads).
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: String,
    pub flags: BTreeSet<String>,
    pub types: Vec<(String, BTreeSet<String>)>,
}

impl Port {
    pub fn describe_types(&self) -> Value {
        json!(self.types.iter().map(|(t, s)| json!([t, s.iter().collect::<Vec<_>>()])).collect::<Vec<_>>())
    }
}

impl<'a> Weapons<'a> {
    pub fn new(items: &'a ItemMaterials, stats: &'a ItemStats<'a>) -> Self {
        Weapons { items, stats, model_cache: Mutex::new(HashMap::new()), pal_rev: OnceLock::new(), ports_cache: Mutex::new(HashMap::new()) }
    }

    fn node_path(v: &Value) -> Option<&str> {
        g(g(v, "Geometry"), "Geometry").get("path")?.as_str()
    }

    /// Model path (`.cdf` / `.cgf` / `.cga`, as written in the record, relative to Data/) of an entity class, or None: the main
    /// `SGeometryResourceParams` geometry, else the first sub-geometry that has one.  Unlike `ItemMaterials::class_geometry` this also
    /// accepts the `.cdf` of first-person weapons.  Cached.
    pub fn model_path(&self, p4k: &p4k::P4k, cls: &str) -> Option<String> {
        if let Some(v) = self.model_cache.lock().unwrap().get(cls) {
            return v.clone();
        }
        let out = self.compute_model_path(p4k, cls);
        self.model_cache.lock().unwrap().insert(cls.to_string(), out.clone());
        out
    }

    fn compute_model_path(&self, p4k: &p4k::P4k, cls: &str) -> Option<String> {
        if cls.is_empty() {
            return None;
        }
        let d = self.items.datacore()?;
        let ids = d.record_index().get(&format!("{}{}", RECORD, cls))?;
        let data = d.record_data(&d.record(*ids.first()?).ok()?, 8).ok()?.to_json();
        for c in arr(g(&data, "Components")) {
            if !truthy(c) || struct_of(c) != "SGeometryResourceParams" {
                continue;
            }
            let node = c.get("Geometry")?;
            let mut path = Self::node_path(node)?.to_string();
            let subs = arr(g(node, "SubGeometry"));
            if path.is_empty() {
                let mut found = String::new();
                for sub in subs {
                    if truthy(sub) {
                        let p = Self::node_path(sub)?;
                        if !p.is_empty() {
                            found = p.to_string();
                            break;
                        }
                    }
                }
                path = found;
            } else if !in_archive(p4k, &path) {
                // EMPIRICAL (survey: lbco_pistol_energy_cen01, an unreleased variant whose own .cdf does not ship): the main path points
                // to a file that is not in the archive while the skin sub-geometries name the base weapon's file.  INFERRED: that file
                // is the right model (a variant only changes materials).  'Destroyed' sub-geometry is never a fallback.
                for sub in subs {
                    if truthy(sub) && !s(g(sub, "Tags")).to_lowercase().contains("destroy") {
                        let p = Self::node_path(sub)?;
                        if in_archive(p4k, p) {
                            path = p.to_string();
                            break;
                        }
                    }
                }
            }
            let low = path.to_lowercase();
            return if MODEL_EXT.iter().any(|e| low.ends_with(e)) { Some(path) } else { None };
        }
        None
    }

    // ---------------------------------------------------------------------------------------------------------- the index
    /// `[{cls, group, type, subtype, size, grade, name, manufacturer, kind, dclass, model, tags, file}]` of every weapon item.
    ///
    /// Only records under a folder whose path mentions weapons, missiles, bombs, turrets or `carryables` / `storage` are opened (the
    /// record table lists a record's file path without reading it).
    pub fn scan(&self, p4k: &p4k::P4k) -> Vec<Value> {
        let Some(d) = self.items.datacore() else { return vec![] };
        let names: Vec<(String, usize)> = ordered_names(self.items, RECORD)
            .into_iter()
            .filter(|(_, i)| {
                let f = d.record_file(*i).map(|f| f.to_lowercase()).unwrap_or_default();
                f.contains("/scitem/weapons/")
                    || f.contains("/ships/weapons")
                    || f.contains("/ships/missile")
                    || f.contains("/ships/turret")
                    || f.contains("/ships/bomb")
                    || f.contains("/carryables/")
                    || f.contains("weaponpersonal")
                    || f.contains("/ships/utility")
            })
            .collect();
        names
            .par_iter()
            .filter_map(|(k, i)| {
                let cls = &k[RECORD.len()..];
                let rec = d.record(*i).ok()?;
                let data = d.record_data(&rec, 4).ok()?.to_json();
                let att = g(comp(&data, "SAttachableComponentParams"), "AttachDef");
                if !truthy(att) || !truthy(g(att, "Type")) {
                    return None;
                }
                let has = |n: &str| arr(g(&data, "Components")).iter().any(|c| truthy(c) && struct_of(c) == n);
                let grp = group_of(s(g(att, "Type")), &has)?;
                let ident = self.stats.identity(cls);
                let desc = self.stats.localise(g(g(att, "Localization"), "Description")).unwrap_or_default();
                let mut row = json!({
                    "cls": cls, "group": grp, "type": g(att, "Type"), "subtype": if truthy(g(att, "SubType")) { g(att, "SubType").clone() } else { json!("") },
                    "size": g(att, "Size"), "grade": g(att, "Grade"), "name": ident["name"], "manufacturer": clean_text(s(&ident["manufacturer"])),
                    "named": ident["name"].as_str() != Some(&pretty(cls)), "kind": description_field(&desc, "kind"),
                    "dclass": description_field(&desc, "dclass"), "model": self.model_path(p4k, cls),
                    "tags": if truthy(g(att, "Tags")) { g(att, "Tags").clone() } else { json!("") }, "file": rec.file
                });
                // carry grips: hands (1|2), grips [{hand, off}], mixed (see carry::grips_of); absent = no grips
                if let Some(Value::Object(h)) = crate::carry::hands_json(&data) {
                    row.as_object_mut().unwrap().extend(h);
                }
                Some(row)
            })
            .collect()
    }

    // ---------------------------------------------------------------------------------------------------------- ports
    /// `{lower-case port name: Port}` of an item record (SItemPortContainerComponentParams), in port order.
    pub fn item_ports(&self, cls: &str) -> std::sync::Arc<Vec<(String, Port)>> {
        if let Some(v) = self.ports_cache.lock().unwrap().get(cls) {
            return v.clone();
        }
        let mut out: Vec<(String, Port)> = vec![];
        if !cls.is_empty() {
            if let Some(data) = self.stats.record(cls, 8) {
                let pc = comp(&data, "SItemPortContainerComponentParams");
                for p in arr(g(pc, "Ports")) {
                    let types: Vec<(String, BTreeSet<String>)> = arr(g(p, "Types"))
                        .iter()
                        .filter(|t| truthy(t))
                        .map(|t| (s(g(t, "Type")).to_string(), arr(g(t, "SubTypes")).iter().filter(|x| truthy(x)).map(|x| pystr(x).to_lowercase()).collect()))
                        .filter(|(t, _): &(String, BTreeSet<String>)| !t.is_empty())
                        .collect();
                    let flags: BTreeSet<String> = s(g(p, "Flags")).split_whitespace().map(|t| t.trim_start_matches('$').to_lowercase()).filter(|t| !t.is_empty()).collect();
                    let name = s(g(p, "Name")).to_string();
                    let port = Port { name: name.clone(), flags, types };
                    let key = name.to_lowercase();
                    match out.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = port,
                        None => out.push((key, port)),
                    }
                }
            }
        }
        let arc = std::sync::Arc::new(out);
        self.ports_cache.lock().unwrap().insert(cls.to_string(), arc.clone());
        arc
    }

    /// Entity class name of a default-loadout entry (JSON form of `ItemMaterials::entry_class`).
    pub fn entry_class(&self, e: &Value) -> String {
        let n = g(e, "entityClassName");
        if truthy(n) {
            return pystr(n);
        }
        if let Some(name) = self.stats.rname(g(e, "entityClassReference")) {
            return name.strip_prefix(RECORD).map(str::to_owned).unwrap_or_default();
        }
        String::new()
    }

    /// The default-loadout entries of an item record (`SEntityComponentDefaultLoadoutParams.loadout.entries`), `[]` when none.
    pub fn default_entries(&self, cls: &str) -> Vec<Value> {
        let Some(data) = self.stats.record(cls, 12) else { return vec![] };
        let mut entries = vec![];
        for c in arr(g(&data, "Components")) {
            if truthy(c) && struct_of(c) == "SEntityComponentDefaultLoadoutParams" {
                entries = arr(g(g(c, "loadout"), "entries")).to_vec();
            }
        }
        entries
    }

    /// Classes sitting on the ports of `cls` and everything on them (the port state of `weaponmodel` without models, default load): the
    /// default item of each port that lists item types or has a default, down to MAX_DEPTH levels, in port order (pre-order).
    pub fn carried(&self, cls: &str, depth: usize, out: &mut Vec<String>) {
        let table = self.item_ports(cls);
        let mut defaults: HashMap<String, String> = HashMap::new();
        for e in self.default_entries(cls) {
            defaults.insert(s(g(&e, "itemPortName")).to_lowercase(), self.entry_class(&e));
        }
        for (name, port) in table.iter() {
            if port.types.is_empty() && !defaults.contains_key(name) {
                continue;
            }
            let cur = defaults.get(name).cloned().unwrap_or_default();
            if !cur.is_empty() {
                out.push(cur.clone());
                if depth + 1 < MAX_DEPTH {
                    self.carried(&cur, depth + 1, out);
                }
            }
        }
    }

    /// Sets `base` (class of the weapon it is a variant of, or null) and `skins` (number of variants of a base) on every entry (in place).
    /// Variants share their base's model and extend its class name by `_<suffix>`; the base of a variant is the entry with the longest
    /// class name that is a prefix (`name + '_'`) of it among the entries with the same model.  Entries without a model are never variants.
    pub fn link_variants(entries: &mut [Value]) {
        let mut by_model: Vec<(String, Vec<usize>)> = vec![];
        let mut base: Vec<Option<String>> = vec![None; entries.len()];
        let mut skins: Vec<i64> = vec![0; entries.len()];
        for (i, e) in entries.iter().enumerate() {
            if truthy(g(e, "model")) {
                let m = s(g(e, "model")).to_lowercase();
                match by_model.iter_mut().find(|(k, _)| *k == m) {
                    Some(grp) => grp.1.push(i),
                    None => by_model.push((m, vec![i])),
                }
            }
        }
        for (_, mut grp) in by_model {
            grp.sort_by_key(|&i| s(g(&entries[i], "cls")).chars().count());
            for (pos, &i) in grp.iter().enumerate() {
                let low = s(g(&entries[i], "cls")).to_lowercase();
                let mut best: Option<usize> = None;
                for &b in &grp[..pos] {
                    let bc = s(g(&entries[b], "cls"));
                    if low.starts_with(&format!("{}_", bc.to_lowercase())) && base[b].is_none() {
                        if best.map_or(true, |x| bc.chars().count() > s(g(&entries[x], "cls")).chars().count()) {
                            best = Some(b);
                        }
                    }
                }
                if let Some(b) = best {
                    base[i] = Some(s(g(&entries[b], "cls")).to_string());
                    skins[b] += 1;
                }
            }
        }
        for (i, e) in entries.iter_mut().enumerate() {
            e["base"] = base[i].take().map_or(Value::Null, |b| json!(b));
            e["skins"] = json!(skins[i]);
        }
    }

    /// Sets `dmg` ({type: damage of one shot}), `dtypes` (types with non-zero damage, largest first), `dom` (dominant type or
    /// 'unknown'), `alpha` / `dps` (null when unknown) and `csizes` (sorted sizes of the guns / missiles carried by default) on every
    /// entry, in place, so the index can filter and sort by them; plus `dmodel` / `alpha_max` / `dps_max` for non-constant damage.
    ///
    /// Guns, missiles and personal weapons read their own ammunition (`WeaponStats::damage_by_type`).  Racks and turrets carry their
    /// payload as default-loadout items: their damage types are the union over the carried guns / missiles, the values summed (INFERRED:
    /// one salvo of every carried item), `csizes` their AttachDef sizes.  Melee has no ammunition: 'unknown'.
    pub fn add_damage(&self, entries: &mut [Value]) {
        let ws = WeaponStats::new(self.items, self.stats);
        type Shot = (Vec<(String, f64)>, Option<f64>, Option<f64>);
        let by_cls: HashMap<String, usize> = entries.iter().enumerate().map(|(i, e)| (s(g(e, "cls")).to_string(), i)).collect();
        // carried classes of racks / turrets, then every class whose own shot is needed, computed once and in parallel
        let carried: Vec<Vec<String>> = entries
            .par_iter()
            .map(|e| {
                let mut v = vec![];
                if matches!(s(g(e, "group")), "rack" | "turret") {
                    self.carried(s(g(e, "cls")), 0, &mut v);
                }
                v
            })
            .collect();
        let mut need: BTreeSet<String> = BTreeSet::new();
        for (e, c) in entries.iter().zip(&carried) {
            match s(g(e, "group")) {
                "rack" | "turret" => {
                    for x in c {
                        if let Some(&ci) = by_cls.get(x) {
                            if matches!(s(g(&entries[ci], "group")), "ship_gun" | "missile") {
                                need.insert(x.clone());
                            }
                        }
                    }
                }
                "melee" => {}
                _ => {
                    need.insert(s(g(e, "cls")).to_string());
                }
            }
        }
        let need: Vec<String> = need.into_iter().collect();
        let shots: HashMap<String, Shot> = need
            .par_iter()
            .map(|c| {
                let dmg = ws.damage_by_type(c, None);
                let m = ws.metrics(c, &[], None);
                (c.clone(), (dmg, m.get("alpha").and_then(num), m.get("dps").and_then(num)))
            })
            .collect();
        let models: HashMap<String, Vec<Value>> = entries
            .par_iter()
            .filter(|e| matches!(s(g(e, "group")), "fps" | "ship_gun"))
            .map(|e| (s(g(e, "cls")).to_string(), ws.damage_models(s(g(e, "cls")), &[], None)))
            .collect();
        let sizes: Vec<Value> = entries.iter().map(|e| g(e, "size").clone()).collect();
        let groups: Vec<String> = entries.iter().map(|e| s(g(e, "group")).to_string()).collect();
        for (idx, e) in entries.iter_mut().enumerate() {
            let group = s(g(e, "group")).to_string();
            let mut csizes: Vec<i64> = vec![];
            let (d, a, p): (Vec<(String, f64)>, Option<f64>, Option<f64>);
            if group == "rack" || group == "turret" {
                let mut dmg: Vec<(String, f64)> = vec![];
                let (mut alpha, mut dps) = (0.0, 0.0);
                let mut sz: BTreeSet<i64> = BTreeSet::new();
                for c in &carried[idx] {
                    let Some(&ci) = by_cls.get(c) else { continue };
                    if !matches!(groups[ci].as_str(), "ship_gun" | "missile") {
                        continue;
                    }
                    let Some(sh) = shots.get(c) else { continue };
                    for (t, v) in &sh.0 {
                        match dmg.iter_mut().find(|(k, _)| k == t) {
                            Some(slot) => slot.1 += v,
                            None => dmg.push((t.clone(), *v)),
                        }
                    }
                    alpha += sh.1.unwrap_or(0.0);
                    dps += sh.2.unwrap_or(0.0);
                    if let Some(sz0) = num(&sizes[ci]) {
                        sz.insert(sz0 as i64);
                    }
                }
                csizes = sz.into_iter().collect();
                d = dmg;
                a = if alpha != 0.0 { Some(alpha) } else { None };
                p = if dps != 0.0 { Some(dps) } else { None };
            } else if group == "melee" {
                d = vec![];
                a = None;
                p = None;
            } else {
                let sh = shots.get(s(g(e, "cls"))).cloned().unwrap_or_default();
                d = sh.0;
                a = sh.1;
                p = sh.2;
            }
            let c = classify_damage(&d);
            e["csizes"] = json!(csizes);
            e["dmg"] = c["values"].clone();
            e["dtypes"] = c["types"].clone();
            e["dom"] = c["dominant"].clone();
            e["alpha"] = jopt(a);
            e["dps"] = jopt(p);
            e["dmodel"] = Value::Null;
            e["alpha_max"] = Value::Null;
            e["dps_max"] = Value::Null;
            if group == "fps" || group == "ship_gun" {
                if let Some(ms) = models.get(s(g(e, "cls"))) {
                    if let Some(top) = ms.first() {
                        // the heat model comes first; a weapon with both would need two labels (none exists)
                        e["dmodel"] = json!(if s(&top["kind"]) == "ramp" { "ramp" } else { "charge" });
                        let flat = truthy(g(top, "gated")) && jeq(&top["alpha_max"], &top["alpha_min"]);
                        if !flat {
                            e["alpha_max"] = top["alpha_max"].clone();
                            e["dps_max"] = top["dps_max"].clone();
                        }
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------------------------- skins
    fn pal_rev(&self) -> &HashMap<usize, String> {
        self.pal_rev.get_or_init(|| self.items.palette_files().iter().map(|(k, i)| (*i, k.clone())).collect())
    }

    /// Key `ItemMaterials::palette` understands (name relative to the palettes folder, lower case, optional '|child') for a
    /// `TintPaletteRef` dict, or None when it is unset or not a TintPaletteTree.
    pub fn palette_key(&self, palette: &Value) -> Option<String> {
        if !palette.is_object() {
            return None;
        }
        let rr = g(palette, "RootRecord");
        let name = self.stats.rname(rr)?;
        if !name.starts_with("TintPaletteTree.") {
            return None;
        }
        let i = self.items.guid_record(rr[2].as_str()?)?;
        let mut key = self.pal_rev().get(&i)?.clone();
        if truthy(g(palette, "ChildPath")) {
            key = format!("{}|{}", key, pystr(g(palette, "ChildPath")).to_lowercase());
        }
        Some(key)
    }

    fn node_info(&self, node: &Value) -> (Option<String>, Option<String>, String) {
        let gm = g(node, "Geometry");
        let mat = s(g(g(gm, "Material"), "path")).replace('\\', "/");
        (if mat.is_empty() { None } else { Some(mat) }, self.palette_key(g(gm, "Palette")), s(g(g(gm, "Geometry"), "path")).replace('\\', "/"))
    }

    fn geo_node(&self, cls: &str) -> Option<(Value, bool)> {
        let data = self.stats.record(cls, 9)?;
        let mut node = None;
        for c in arr(g(&data, "Components")) {
            if truthy(c) && struct_of(c) == "SGeometryResourceParams" {
                node = Some((g(c, "Geometry").clone(), truthy(g(c, "rootOverridePaint"))));
            }
        }
        node.filter(|n| truthy(&n.0))
    }

    /// `[{id, name, tag, material, palette, cls, default}]` of a weapon: the default look (the main node of its record) first, then one
    /// entry per `SubGeometry` node that changes the material or the palette (nodes that only switch the model, like a magazine's
    /// 'tableDisplay' / 'empty', are not skins).  `material` / `palette` are the effective values (an empty node field inherits the main
    /// node's); `cls` = the variant class that sells that skin (`<cls>_<tag>`, any case) or null.  `variants` = classes of the variants.
    pub fn skins(&self, cls: &str, variants: &[String]) -> Vec<Value> {
        let Some((node, _)) = self.geo_node(cls) else { return vec![] };
        let main = self.node_info(&node);
        let mut out = vec![json!({"id": "", "name": "Default", "tag": "", "material": main.0, "palette": main.1, "cls": cls, "default": true})];
        let mut seen: Vec<(Option<String>, Option<String>)> = vec![(main.0.clone(), main.1.clone())];
        let low: HashMap<String, &String> = variants.iter().map(|v| (v.to_lowercase(), v)).collect();
        for sub in arr(g(&node, "SubGeometry")) {
            if !truthy(sub) {
                continue;
            }
            let info = self.node_info(sub);
            if info.2.to_lowercase() != main.2.to_lowercase() {
                continue;
            }
            let mat = info.0.clone().or_else(|| main.0.clone());
            let pal = info.1.clone().or_else(|| main.1.clone());
            let tag = s(g(sub, "Tags")).trim().to_string();
            if tag.is_empty() || seen.contains(&(mat.clone(), pal.clone())) {
                continue;
            }
            seen.push((mat.clone(), pal.clone()));
            let vcls = low.get(&format!("{}_{}", cls, tag).to_lowercase()).map(|v| v.to_string());
            out.push(json!({"id": tag.to_lowercase(), "name": self.skin_name(vcls.as_deref(), &tag), "tag": tag, "material": mat, "palette": pal, "cls": vcls, "default": false}));
        }
        let used: Vec<Option<String>> = out.iter().map(|o| o["cls"].as_str().map(str::to_owned)).collect();
        let mut sorted: Vec<&String> = variants.iter().collect();
        sorted.sort();
        for v in sorted {
            if used.contains(&Some(v.clone())) || !v.to_lowercase().starts_with(&format!("{}_", cls.to_lowercase())) {
                continue;
            }
            // A variant class that is not a SubGeometry tag can carry its own look in its main node (EMPIRICAL: ksar_smg_energy_01_red01,
            // gmni_smg_ballistic_01_luminalia): it is a skin too.  Variants with the same look (test / AI / reward copies) are not.
            let Some((vnode, _)) = self.geo_node(v) else { continue };
            let info = self.node_info(&vnode);
            if info.2.to_lowercase() != main.2.to_lowercase() {
                continue;
            }
            let mat = info.0.clone().or_else(|| main.0.clone());
            let pal = info.1.clone().or_else(|| main.1.clone());
            if seen.contains(&(mat.clone(), pal.clone())) {
                continue;
            }
            seen.push((mat.clone(), pal.clone()));
            let tag: String = v.chars().skip(cls.chars().count() + 1).collect();
            out.push(json!({"id": tag.to_lowercase(), "name": self.skin_name(Some(v), &tag), "tag": tag, "material": mat, "palette": pal, "cls": v, "default": false}));
        }
        out
    }

    /// `{material, palette, tagged, override}` of the item `cls` (an attachment) when the weapon it hangs on wears the skin `tag`
    /// ('' = default).  `host` = `{material, palette}` of the item it is attached to, used only when the item has `rootOverridePaint`.
    ///
    /// CONFIRMED against the engine (see the module doc): the child uses ITS OWN matching node's `Palette` / `Material`; a `TintPaletteRef`
    /// without `RootRecord` is a null palette (no fallback to the parent); `rootOverridePaint` makes the child take the host's.
    /// EMPIRICAL: 30 of the weapons' 88 skin tags occur on attachments.
    pub fn item_look(&self, cls: &str, tag: &str, host: Option<&Value>) -> Value {
        let Some((node, over)) = self.geo_node(cls) else { return json!({"material": null, "palette": null, "tagged": false, "override": false}) };
        let main = self.node_info(&node);
        let mut out = json!({"material": null, "palette": main.1, "tagged": false, "override": over});
        let want = tag.trim().to_lowercase();
        for sub in arr(g(&node, "SubGeometry")) {
            if !want.is_empty() && truthy(sub) && s(g(sub, "Tags")).to_lowercase().split_whitespace().any(|t| t == want) {
                let info = self.node_info(sub);
                if !info.2.is_empty() && info.2.to_lowercase() != main.2.to_lowercase() {
                    continue;
                }
                out["material"] = json!(info.0);
                out["palette"] = json!(info.1.or(main.1.clone()));
                out["tagged"] = json!(true);
                break;
            }
        }
        if over {
            if let Some(h) = host {
                if truthy(g(h, "material")) {
                    out["material"] = g(h, "material").clone();
                }
                if truthy(g(h, "palette")) {
                    out["palette"] = g(h, "palette").clone();
                }
            }
        }
        out
    }

    /// Display name of a skin: the quoted name in its variant's localised name ('P4-AR "Desert Shadow" Rifle' -> 'Desert Shadow'), else
    /// that name without the base's, else the tag prettified ('Tan01' -> 'Tan 01').
    pub fn skin_name(&self, vcls: Option<&str>, tag: &str) -> String {
        if let Some(v) = vcls.filter(|v| !v.is_empty()) {
            let n = self.stats.identity(v)["name"].as_str().unwrap_or("").to_string();
            static Q: OnceLock<Regex> = OnceLock::new();
            let q = Q.get_or_init(|| Regex::new("[\"\u{201c}]([^\"\u{201d}]+)[\"\u{201d}]").unwrap());
            if let Some(m) = q.captures(&n) {
                return m[1].to_string();
            }
            if !n.is_empty() && n != pretty(v) {
                return n;
            }
        }
        // re.sub(r"(?<=[A-Za-z])(?=\d)|_", " ", tag).strip()
        let mut out = String::new();
        let mut prev: Option<char> = None;
        for c in tag.chars() {
            if c == '_' {
                out.push(' ');
            } else {
                if c.is_ascii_digit() && prev.map_or(false, |p| p.is_ascii_alphabetic()) {
                    out.push(' ');
                }
                out.push(c);
            }
            prev = Some(c);
        }
        let pretty_t = out.trim().to_string();
        let mut it = pretty_t.chars();
        match it.next() {
            Some(c) => c.to_uppercase().collect::<String>() + it.as_str(),
            None => String::new(),
        }
    }
}

fn pretty(cls: &str) -> String {
    crate::itemstats::pretty(cls)
}

/// Readable name of a port: from its first item sub-type (`ironsight` -> Optic), else its name prettified.
pub fn port_label(port: &Port) -> String {
    for (_t, subs) in &port.types {
        for sub in subs {
            if let Some((_, l)) = ATTACH_LABELS.iter().find(|(k, _)| k == sub) {
                return l.to_string();
            }
        }
    }
    static A: OnceLock<Regex> = OnceLock::new();
    static B: OnceLock<Regex> = OnceLock::new();
    let a = A.get_or_init(|| Regex::new(r"(?i)_?(attach|attachment)$").unwrap());
    let b = B.get_or_init(|| Regex::new(r"[_\s]+").unwrap());
    let t = a.replace(&port.name, "");
    let t = b.replace_all(&t, " ").trim().to_string();
    let mut it = t.chars();
    let cap = match it.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &it.as_str().to_lowercase(),
        None => String::new(),
    };
    if cap.is_empty() {
        port.name.clone()
    } else {
        cap
    }
}

/// True when the player can put something on the port: it lists item types and is not an `uneditable` one (INFERRED).
pub fn attachable(port: &Port) -> bool {
    !port.types.is_empty() && !port.flags.contains("uneditable")
}

/// True for an entry the index shows: a base weapon (not a variant) with a localised name and a model, that is not a template / test /
/// placeholder record.
pub fn is_listed(e: &Value) -> bool {
    g(e, "base").is_null() && truthy(g(e, "model")) && truthy(g(e, "named")) && !re_skip().is_match(s(g(e, "cls")))
}

/// Build the whole weapon index (scan, variants, damage).
pub fn build_index(items: &ItemMaterials, stats: &ItemStats, p4k: &p4k::P4k) -> Vec<Value> {
    let w = Weapons::new(items, stats);
    let mut data = w.scan(p4k);
    Weapons::link_variants(&mut data);
    w.add_damage(&mut data);
    data
}
