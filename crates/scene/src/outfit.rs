//! Outfit builder rules: which wearable can be worn together, from the item ports of the player body and of the items themselves.
//! Evidence (EMPIRICAL on the live archive):
//!
//! * The player body records (`Char_Body`, picked as the body whose `Char_Head` port accepts the sex) own the ports `Armor_Undersuit`,
//!   `Clothing_Torso_0/1`, `Clothing_Legs`, `Clothing_Feet`, `Clothing_Hands`; armor hangs on the UNDERSUIT (`armor_torso` / `armor_arms` /
//!   `armor_legs` / `armor_helmet` / `backpack`); hats and eyewear hang on the HEAD record with the most wearable ports (INFERRED).
//! * Ports flagged `heldentity` / `attach_helmet` / `inventoryinternalonly` / `invisible` hold carried things, not worn ones (`CARRY_FLAGS`).
//! * `SCItemClothingParams.HiddenParts` lists ports an item HIDES (INFERRED: a hidden item is not drawn); `protectedBodyParts` -> `jointName`
//!   gives the body regions an item covers.
//!
//! The rule: an item is wearable when some free, non-carry port of the body, the head or an already worn item accepts its `AttachDef.Type`
//! (and its sub-type when the port lists some) and its size lies in the port's `MinSize..MaxSize` (0 = no limit; INFERRED).  Items are placed
//! repeatedly until nothing more fits.
//!
//! Rows are `serde_json::Value` dicts (the page and the routes consume them as JSON).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::dynv::{arr, g, s, struct_of, truthy};
use crate::gear;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::{comp, ItemStats, RECORD};
use crate::swaps::{self, Port};
use crate::weapons::ordered_names;

pub const CACHE_VERSION: i64 = 1;
pub const CARRY_FLAGS: [&str; 4] = ["heldentity", "attach_helmet", "inventoryinternalonly", "invisible"];
pub const HEAD_TYPE: &str = "Char_Head";
pub const BODY_TYPE: &str = "Char_Body";
pub const FOLDER: &str = "/scitem/characters/";
pub const SEXES: [&str; 2] = ["male", "female"];
pub const REGIONS: [&str; 6] = ["Head", "Torso", "LeftArm", "RightArm", "LeftLeg", "RightLeg"];

/// JSON row of a port keeping only the `Char_*` types: {name, types: [[type, [sub types]]], min, max, flags}.
pub fn port_row(port: &Port) -> Value {
    let types: Vec<Value> = port.types.iter().filter(|(t, _)| t.starts_with("Char_")).map(|(t, subs)| json!([t, subs.iter().collect::<Vec<_>>()])).collect();
    json!({"name": port.name, "types": types, "min": port.min, "max": port.max, "flags": port.flags.iter().collect::<Vec<_>>()})
}

/// The port rows of the item ports of `cls` that accept at least one `Char_*` type.
pub fn char_ports(stats: &ItemStats, cls: &str) -> Vec<Value> {
    swaps::item_ports(stats, cls)
        .iter()
        .map(|(_, p)| port_row(p))
        .filter(|r| !arr(&r["types"]).is_empty() && !s(&r["name"]).is_empty() && !s(&r["name"]).to_lowercase().starts_with("weapon_attach"))
        .collect()
}

fn or_zero(v: &Value) -> Value {
    if truthy(v) {
        v.clone()
    } else {
        json!(0.0)
    }
}

/// Rules of one gear index entry: {cls, type, subtype, size, ports, hides, covers, zones, sig}.
pub fn item_row(stats: &ItemStats, entry: &Value) -> Value {
    let cls = s(g(entry, "cls"));
    let data = stats.record(cls, 14).unwrap_or(Value::Null);
    let cl = comp(&data, "SCItemClothingParams");
    let arm = comp(&data, "SCItemSuitArmorParams");
    let parts: Vec<Option<Value>> = arr(g(arm, "protectedBodyParts")).iter().map(|p| stats.rref(p, 4)).collect();
    let hides: BTreeSet<String> = arr(g(cl, "HiddenParts")).iter().filter(|h| truthy(h) && truthy(g(h, "PortName"))).map(|h| s(g(h, "PortName")).to_lowercase()).collect();
    let covers: BTreeSet<String> = parts.iter().flatten().filter(|p| truthy(g(p, "jointName"))).map(|p| s(g(p, "jointName")).to_string()).collect();
    let zones: BTreeSet<String> = arr(g(cl, "Chunks")).iter().filter(|c| truthy(c) && truthy(g(c, "MeshChunk"))).map(|c| s(g(c, "MeshChunk")).to_string()).collect();
    let sig: Vec<Value> = arr(g(arm, "signatureParams"))
        .iter()
        .filter(|x| truthy(x))
        .map(|x| json!([g(x, "signatureType"), or_zero(g(x, "signatureEmission")), or_zero(g(x, "signatureReductionWeighted")), or_zero(g(x, "signatureReductionAbsolute"))]))
        .collect();
    json!({"cls": cls, "type": g(entry, "type"), "subtype": s(g(entry, "subtype")).to_lowercase(), "size": g(entry, "size"), "ports": char_ports(stats, cls),
        "hides": hides, "covers": covers, "zones": zones, "sig": sig})
}

/// {"male": host, "female": host, "head": host} with host = {cls, ports}: the body records and the head record with the most wearable ports.
pub fn find_hosts(items: &ItemMaterials, stats: &ItemStats) -> Value {
    let Some(d) = items.datacore() else { return json!({}) };
    let (mut bodies, mut heads): (Vec<String>, Vec<String>) = (vec![], vec![]);
    for (k, i) in ordered_names(items, RECORD) {
        if !d.record_file(i).map(|f| f.to_lowercase().contains(FOLDER)).unwrap_or(false) {
            continue;
        }
        let Ok(rec) = d.record(i) else { continue };
        let Ok(data) = d.record_data(&rec, 4) else { continue };
        let data = data.to_json();
        let att = arr(g(&data, "Components")).iter().find(|c| truthy(c) && struct_of(c) == "SAttachableComponentParams").map(|c| g(c, "AttachDef").clone()).unwrap_or(Value::Null);
        let t = s(g(&att, "Type"));
        if t == BODY_TYPE || (t == HEAD_TYPE && s(g(&att, "SubType")) != "Medical") {
            (if t == BODY_TYPE { &mut bodies } else { &mut heads }).push(k[RECORD.len()..].to_string());
        }
    }
    let mut out = Map::new();
    for sex in SEXES {
        let mut best: Option<((i64, usize, String), String, Vec<Value>)> = None;
        for cls in &bodies {
            let ports = char_ports(stats, cls);
            let head = ports.iter().any(|p| arr(&p["types"]).iter().any(|t| s(&t[0]) == HEAD_TYPE && arr(&t[1]).iter().any(|x| s(x) == sex)));
            if head && !ports.is_empty() {
                let key = (-(ports.len() as i64), cls.chars().count(), cls.clone());
                if best.as_ref().map_or(true, |b| key < b.0) {
                    best = Some((key, cls.clone(), ports));
                }
            }
        }
        if let Some(b) = best {
            out.insert(sex.into(), json!({"cls": b.1, "ports": b.2}));
        }
    }
    let mut best: Option<((i64, usize, String), String, Vec<Value>)> = None;
    for cls in &heads {
        let ports = char_ports(stats, cls);
        let n = ports.iter().filter(|p| arr(&p["types"]).iter().any(|t| s(&t[0]) == "Char_Clothing_Hat" || s(&t[0]) == "Char_Accessory_Eyes")).count();
        let key = (-(n as i64), cls.chars().count(), cls.clone());
        if n > 0 && best.as_ref().map_or(true, |b| key < b.0) {
            best = Some((key, cls.clone(), ports));
        }
    }
    if let Some(b) = best {
        out.insert("head".into(), json!({"cls": b.1, "ports": b.2}));
    }
    Value::Object(out)
}

/// {"items": {class: item_row}, "hosts": find_hosts, "version"} for the listed base entries of the gear index.
pub fn build_rules(items: &ItemMaterials, stats: &ItemStats, entries: &[&Value]) -> Value {
    let mut rows = Map::new();
    for e in entries {
        rows.insert(s(g(e, "cls")).to_string(), item_row(stats, e));
    }
    json!({"version": CACHE_VERSION, "hosts": find_hosts(items, stats), "items": rows})
}

// ---------------------------------------------------------------------------------------------------------------- the rule
/// The ports of a host row that hold worn items: all but the carry ports.
pub fn worn_ports(host: &Value) -> Vec<&Value> {
    arr(g(host, "ports")).iter().filter(|p| !arr(g(p, "flags")).iter().any(|f| CARRY_FLAGS.contains(&s(f)))).collect()
}

/// True when `port` takes `item` ({type, subtype, size}): type listed (and its sub-type when the type lists some), size inside MinSize..MaxSize.
pub fn accepts(port: &Value, item: &Value) -> bool {
    size_ok(port, item) && type_ok(port, item)
}

/// Type listed, and the item's sub-type among the listed ones.  No listed sub-type = any; an item sub-type of "" / "undefined" matches any
/// listed one (the rule `swaps` uses for ship items, INFERRED).
pub fn type_ok(port: &Value, item: &Value) -> bool {
    let sub = s(g(item, "subtype")).to_lowercase();
    arr(g(port, "types")).iter().any(|t| {
        let subs = arr(&t[1]);
        s(&t[0]) == s(g(item, "type")) && (subs.is_empty() || subs.iter().any(|x| s(x) == sub) || sub.is_empty() || sub == "undefined")
    })
}

pub fn size_ok(port: &Value, item: &Value) -> bool {
    let size = g(item, "size").as_i64();
    let max = g(port, "max").as_i64().unwrap_or(0);
    max <= 0 || size.is_none() || (g(port, "min").as_i64().unwrap_or(0) <= size.unwrap() && size.unwrap() <= max)
}

pub fn slot_order() -> Vec<&'static str> {
    gear::SLOTS.iter().map(|x| x.0).collect()
}

/// Place the chosen items on the body.  `selection` = {slot: class}, `catalog` = {class: item_row + "slot"}, `hosts` = find_hosts output.
///
/// Returns {placed: {slot: {host: "body" | "head" | slot of the carrier, port}}, blocked: {slot: {kind, ...}}, hidden: {slot: [slots that hide it]}}.
/// A blocked reason is {kind: "size", host, port, accepts: [min, max]}, {kind: "taken", host, port} or {kind: "port"}.
pub fn resolve(selection: &HashMap<String, String>, catalog: &Map<String, Value>, hosts: &Value, sex: &str) -> Value {
    let mut todo: Vec<&str> = slot_order().into_iter().filter(|sl| selection.get(*sl).map_or(false, |c| catalog.contains_key(c))).collect();
    let empty = json!({"ports": []});
    let mut hostmap: Vec<(String, Value)> = vec![
        ("body".into(), if truthy(g(hosts, sex)) { g(hosts, sex).clone() } else { empty.clone() }),
        ("head".into(), if truthy(g(hosts, "head")) { g(hosts, "head").clone() } else { empty.clone() }),
    ];
    let mut placed: Vec<(String, Value)> = vec![];
    let mut used: HashSet<(String, String)> = HashSet::new();
    let mut progress = true;
    while progress && !todo.is_empty() {
        progress = false;
        for slot in todo.clone() {
            let it = &catalog[&selection[slot]];
            let mut spot: Option<(String, Value)> = None;
            'outer: for (hk, host) in &hostmap {
                for p in worn_ports(host) {
                    if !used.contains(&(hk.clone(), s(g(p, "name")).to_lowercase())) && accepts(p, it) {
                        spot = Some((hk.clone(), p.clone()));
                        break 'outer;
                    }
                }
            }
            if let Some((hk, p)) = spot {
                used.insert((hk.clone(), s(g(&p, "name")).to_lowercase()));
                placed.push((slot.to_string(), json!({"host": hk, "port": g(&p, "name")})));
                hostmap.push((slot.to_string(), json!({"ports": g(it, "ports"), "cls": g(it, "cls")})));
                todo.retain(|x| *x != slot);
                progress = true;
            }
        }
    }
    let mut blocked = Map::new();
    for slot in &todo {
        let it = &catalog[&selection[*slot]];
        let mut reason = json!({"kind": "port"});
        for (hk, host) in &hostmap {
            for p in worn_ports(host) {
                if type_ok(p, it) {
                    if !size_ok(p, it) {
                        reason = json!({"kind": "size", "host": hk, "port": g(p, "name"), "accepts": [g(p, "min"), g(p, "max")]});
                    } else if used.contains(&(hk.clone(), s(g(p, "name")).to_lowercase())) && s(&reason["kind"]) == "port" {
                        reason = json!({"kind": "taken", "host": hk, "port": g(p, "name")});
                    }
                }
            }
        }
        blocked.insert(slot.to_string(), reason);
    }
    let mut hidden = Map::new();
    for (slot, at) in &placed {
        let port = s(g(at, "port")).to_lowercase();
        for (other, _) in &placed {
            if other != slot && arr(g(&catalog[&selection[other]], "hides")).iter().any(|h| s(h) == port) {
                hidden.entry(slot.clone()).or_insert_with(|| json!([])).as_array_mut().unwrap().push(json!(other));
            }
        }
    }
    let mut pl = Map::new();
    for (k, v) in placed {
        pl.insert(k, v);
    }
    json!({"placed": pl, "blocked": blocked, "hidden": hidden})
}

/// Slots whose items offer a worn port for `item_type`: {slot: number of items with such a port}.
pub fn providers(catalog: &Map<String, Value>, item_type: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for it in catalog.values() {
        if worn_ports(it).iter().any(|p| type_ok(p, &json!({"type": item_type}))) {
            *out.entry(s(g(it, "slot")).to_string()).or_insert(0) += 1;
        }
    }
    out
}

/// What the rule looks at in an item: type, sub-type, size, worn ports and hidden ports (as a string key).
pub fn signature(it: &Value) -> String {
    format!("{}|{}|{}|{}|{}|{}", s(g(it, "slot")), s(g(it, "type")), g(it, "subtype"), g(it, "size"), g(it, "ports"), g(it, "hides"))
}

/// {slot: [class]}: one item per distinct `signature`.
pub fn representatives(catalog: &Map<String, Value>) -> BTreeMap<String, Vec<String>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut keys: Vec<&String> = catalog.keys().collect();
    keys.sort();
    for cls in keys {
        let it = &catalog[cls];
        if seen.insert(signature(it)) {
            out.entry(s(g(it, "slot")).to_string()).or_default().push(cls.clone());
        }
    }
    out
}

/// {(slot a, slot b): {free, hosted, never, hides, total}} over the signature groups of the two slots.
pub fn compat_matrix(catalog: &Map<String, Value>, hosts: &Value, sex: &str) -> Vec<((String, String), Value)> {
    let reps = representatives(catalog);
    let order = slot_order();
    let mut slots: Vec<&String> = reps.keys().collect();
    slots.sort_by_key(|x| order.iter().position(|o| o == x).unwrap_or(99));
    let carriers: BTreeMap<&String, Vec<&String>> = reps.iter().map(|(sl, cs)| (sl, cs.iter().filter(|c| !worn_ports(&catalog[*c]).is_empty()).collect())).collect();
    let mut out = vec![];
    for (i, a) in slots.iter().enumerate() {
        for b in &slots[i + 1..] {
            let (mut free, mut hosted, mut never, mut hides, mut total) = (0, 0, 0, 0, 0);
            let mut extras: Vec<(&String, &String)> = vec![];
            for (sl, cs) in &carriers {
                if sl != a && sl != b {
                    for c in cs {
                        extras.push((sl, c));
                    }
                }
            }
            for x in &reps[*a] {
                for y in &reps[*b] {
                    total += 1;
                    let mut base: HashMap<String, String> = HashMap::new();
                    base.insert((*a).clone(), x.clone());
                    base.insert((*b).clone(), y.clone());
                    let r = resolve(&base, catalog, hosts, sex);
                    let mut found: Option<Value> = None;
                    if arr_empty_obj(&r["blocked"]) {
                        free += 1;
                        found = Some(r);
                    } else {
                        for (s1, c1) in &extras {
                            let mut m = base.clone();
                            m.insert((*s1).clone(), (*c1).clone());
                            let r2 = resolve(&m, catalog, hosts, sex);
                            if arr_empty_obj(&r2["blocked"]) {
                                found = Some(r2);
                                break;
                            }
                        }
                        if found.is_none() {
                            'pairs: for (i1, (s1, c1)) in extras.iter().enumerate() {
                                for (s2, c2) in &extras[i1 + 1..] {
                                    if s1 != s2 {
                                        let mut m = base.clone();
                                        m.insert((*s1).clone(), (*c1).clone());
                                        m.insert((*s2).clone(), (*c2).clone());
                                        let r2 = resolve(&m, catalog, hosts, sex);
                                        if arr_empty_obj(&r2["blocked"]) {
                                            found = Some(r2);
                                            break 'pairs;
                                        }
                                    }
                                }
                            }
                        }
                        if found.is_some() {
                            hosted += 1;
                        } else {
                            never += 1;
                        }
                    }
                    if let Some(r) = &found {
                        let ab = [a.as_str(), b.as_str()];
                        if r["hidden"].as_object().map_or(false, |h| h.iter().any(|(k, hs)| ab.contains(&k.as_str()) && arr(hs).iter().any(|x| ab.contains(&s(x))))) {
                            hides += 1;
                        }
                    }
                }
            }
            out.push((((*a).clone(), (*b).clone()), json!({"free": free, "hosted": hosted, "never": never, "hides": hides, "total": total})));
        }
    }
    out
}

fn arr_empty_obj(v: &Value) -> bool {
    v.as_object().map_or(true, |o| o.is_empty())
}

/// The catalog the page works on: {class: row} for every listed base entry, joining the gear numbers with the port rules.  `craftable` =
/// {class: true} or None while crafting data builds.
pub fn catalog_rows(gear_entries: &[&Value], rules: &Value, craftable: Option<&HashSet<String>>) -> Map<String, Value> {
    let mut out = Map::new();
    for e in gear_entries {
        let cls = s(g(e, "cls"));
        let r = g(g(rules, "items"), cls);
        if r.is_null() {
            continue;
        }
        let f = g(e, "facts");
        let mut row = r.as_object().cloned().unwrap_or_default();
        let res = if truthy(g(f, "res")) { g(f, "res").clone() } else { json!({}) };
        for (k, v) in [
            ("slot", g(e, "slot").clone()), ("kind", g(e, "kind").clone()), ("name", g(e, "name").clone()), ("manufacturer", g(e, "manufacturer").clone()),
            ("weight", g(e, "subtype").clone()), ("skins", g(e, "skins").clone()), ("set", g(e, "set").clone()), ("set_name", g(e, "set_name").clone()),
            ("res", res), ("impact", g(f, "impact").clone()), ("tmin", g(f, "tmin").clone()), ("tmax", g(f, "tmax").clone()), ("rad", g(f, "rad").clone()),
            ("radrate", g(f, "radrate").clone()), ("cap", g(f, "cap").clone()), ("mass", g(f, "mass").clone()),
            ("craftable", match craftable { None => Value::Null, Some(c) => json!(c.contains(cls)) }),
        ] {
            row.insert(k.into(), v);
        }
        out.insert(cls.to_string(), Value::Object(row));
    }
    out
}

/// For the survey: {set key: (members by slot, assemblable?, undersuit used)} over the armor sets of the index.
pub fn complete_sets(entries: &[&Value], catalog: &Map<String, Value>, hosts: &Value, sex: &str) -> BTreeMap<String, (BTreeMap<String, String>, bool, Option<String>)> {
    let mut sets: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for e in entries {
        if truthy(g(e, "set")) && catalog.contains_key(s(g(e, "cls"))) {
            sets.entry(s(g(e, "set")).to_string()).or_default().entry(s(g(e, "slot")).to_string()).or_insert_with(|| s(g(e, "cls")).to_string());
        }
    }
    let unders: Vec<&String> = catalog.iter().filter(|(_, it)| s(g(it, "slot")) == "undersuit").map(|(c, _)| c).collect();
    let mut out = BTreeMap::new();
    for (key, members) in sets {
        let (mut ok, mut used) = (false, None);
        let cands: Vec<Option<&String>> = if members.contains_key("undersuit") { vec![None] } else { unders.iter().map(|u| Some(*u)).collect() };
        for u in cands {
            let mut sel: HashMap<String, String> = members.clone().into_iter().collect();
            if let Some(u) = u {
                sel.insert("undersuit".into(), u.clone());
            }
            let r = resolve(&sel, catalog, hosts, sex);
            if arr_empty_obj(&r["blocked"]) {
                ok = true;
                used = u.cloned();
                break;
            }
        }
        out.insert(key, (members, ok, used));
    }
    out
}
