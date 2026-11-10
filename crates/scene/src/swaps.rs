//! Swappable ship components: which item ports accept replacement items, and which items fit.
//!
//! EVIDENCE (live archive)
//! * EMPIRICAL  the ports of a ship are NOT in its entity record (that one lists only 6, all uneditable): they are in the vehicle definition XML
//!   named by `VehicleComponentParams.vehicleDefinition` (`Scripts/Entities/Vehicles/Implementations/Xml/<ship>.xml`, an encrypted CryXmlB the
//!   archive reader decrypts).  Every `<Part name=...><ItemPort minSize maxSize flags requiredTags><Types><Type type subtypes/>` is a port; the part
//!   name is the loadout's `itemPortName`.  The ports of an INSTALLED item are `SItemPortContainerComponentParams.Ports[]` of the item's own record.
//! * EMPIRICAL  flags are space separated tokens; the ones that matter: `editable`, `uneditable`, `invisible`, `swaponly`; a `$` prefix does not
//!   change the meaning.  A port is offered when it has `editable` and not `uneditable` (INFERRED from the names).
//! * INFERRED   compatibility = item Type equals one of the port's Types, the item SubType is among the port's listed subtypes (none listed = any),
//!   the item Size is in [minSize, maxSize], the port's required tags are all among the item's `AttachDef.Tags` and the item's `RequiredTags` are all
//!   among the port's tags or the HOST tags.  An item SubType of UNDEFINED matches any listed subtype.  The host tags of a port are the RequiredTags
//!   of its default item (`host_tags`).  Tags are compared case-insensitively with a leading `$` removed.
//! * The candidate pool is one pass over all EntityClassDefinition records (rayon makes the pass fast enough that no cache is needed).

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use p4k_formats::xmlq;
use p4k_formats::{Error, Result};
use p4k::P4k;
use rayon::prelude::*;
use serde_json::{json, Value};

use crate::itemdata::{comp, ItemData, RECORD};
use crate::itemmaterials::ItemMaterials;

pub type Tags = BTreeSet<String>;

/// Set of lower-case tags of a space/comma separated tag string, a leading `$` removed.
pub fn tags(s: &str) -> Tags {
    s.split(|c: char| c.is_whitespace() || c == ',').map(|t| t.trim_start_matches('$')).filter(|t| !t.is_empty()).map(str::to_lowercase).collect()
}

/// One item port definition (from the vehicle XML or an item record).
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: String,
    pub flags: Tags,
    pub min: i64,
    pub max: i64,
    /// (type, lower-case sub types)
    pub types: Vec<(String, Tags)>,
    pub required: Tags,
    pub tags: Tags,
}

impl Port {
    pub fn new(name: &str, flags: &str, min: i64, max: i64, types: &[(String, Vec<String>)], required: &str, ptags: &str) -> Port {
        Port {
            name: name.to_string(),
            flags: flags.split_whitespace().map(|t| t.trim_start_matches('$')).filter(|t| !t.is_empty()).map(str::to_lowercase).collect(),
            min,
            max,
            types: types.iter().filter(|(t, _)| !t.is_empty()).map(|(t, s)| (t.clone(), s.iter().filter(|x| !x.is_empty()).map(|x| x.to_lowercase()).collect())).collect(),
            required: tags(required),
            tags: tags(ptags),
        }
    }

    pub fn swappable(&self) -> bool {
        self.flags.contains("editable") && !self.flags.contains("uneditable") && !self.types.is_empty()
    }

    pub fn describe(&self) -> Value {
        json!({"name": self.name, "min": self.min, "max": self.max, "types": self.types.iter().map(|(t, s)| json!([t, s])).collect::<Vec<_>>(),
            "flags": self.flags, "required": self.required})
    }
}

/// What the compatibility rule looks at in an item.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolItem {
    pub cls: String,
    pub atype: String,
    pub subtype: String,
    pub size: Option<i64>,
    pub grade: Option<i64>,
    pub tags: Tags,
    pub req: Tags,
}

/// Why `item` does not fit `port` ('type', 'subtype', 'size', 'tags' or 'reqtags'), None when it fits.  `host` = tags the hosting ship provides
/// beyond the port's own.
pub fn mismatch(port: &Port, item: &PoolItem, host: &Tags) -> Option<&'static str> {
    let same: Vec<&(String, Tags)> = port.types.iter().filter(|(t, _)| *t == item.atype).collect();
    if same.is_empty() {
        return Some("type");
    }
    let sub = item.subtype.to_lowercase();
    if !same.iter().any(|(_, subs)| subs.is_empty() || subs.contains(&sub) || item.subtype.is_empty() || item.subtype == "UNDEFINED") {
        return Some("subtype");
    }
    match item.size {
        Some(s) if port.min <= s && s <= port.max => {}
        _ => return Some("size"),
    }
    if !port.required.is_subset(&item.tags) {
        return Some("tags");
    }
    let allowed: Tags = port.tags.union(&port.required).chain(host.iter()).cloned().collect();
    if !item.req.is_subset(&allowed) {
        return Some("reqtags");
    }
    None
}

pub fn compatible(port: &Port, item: &PoolItem, host: &Tags) -> bool {
    mismatch(port, item, host).is_none()
}

fn py_int(s: Option<&str>) -> Result<i64> {
    match s {
        None | Some("") => Ok(0),
        Some(s) => s.trim().parse::<i64>().map_err(|_| Error::msg(format!("ValueError: invalid literal for int(): '{}'", s))),
    }
}

/// Attribute `key` of `e`, the exact spelling first, else any ASCII-case variant.  EMPIRICAL: the vehicle XMLs are hand written and spell the
/// size attributes `minSize`/`maxSize` on most ports but `minsize`/`maxsize` on others (e.g. rsi_aurora_gs.xml: power plant, life support; 9 of
/// 66 size-carrying ports of that file).  Reading only the camel-case spelling gave such a port 0..0, which `mismatch` reads as "size 0 only":
/// the change list of an Aurora power plant held nothing but S0 items although the installed one is S1.
fn get_ci<'a>(e: &'a p4k_formats::cryxml::Element, key: &str) -> Option<&'a str> {
    e.get(key).or_else(|| e.attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str()))
}

fn attr_ci<'a>(e: &'a p4k_formats::cryxml::Element, key: &str) -> &'a str {
    get_ci(e, key).unwrap_or("")
}

/// `{lower-case part name: Port}` of every `<Part name=..><ItemPort ...>` of a vehicle definition (CryXmlB or plain XML bytes).
pub fn parse_vehicle_xml(data: &[u8]) -> Result<HashMap<String, Port>> {
    let root = xmlq::parse_any(data)?;
    let mut out = HashMap::new();
    for part in xmlq::iter(&root, "Part") {
        let Some(ip) = part.children.iter().find(|c| c.tag == "ItemPort") else { continue };
        let Some(name) = part.get("name").filter(|n| !n.is_empty()) else { continue };
        let types: Vec<(String, Vec<String>)> = xmlq::iter(ip, "Type")
            .into_iter()
            .map(|t| (xmlq::attr(t, "type").to_string(), xmlq::attr(t, "subtypes").split(',').map(|s| s.trim().to_string()).collect()))
            .collect();
        out.insert(
            name.to_lowercase(),
            Port::new(name, attr_ci(ip, "flags"), py_int(get_ci(ip, "minSize"))?, py_int(get_ci(ip, "maxSize"))?, &types, attr_ci(ip, "requiredTags"), attr_ci(ip, "portTags")),
        );
    }
    Ok(out)
}

/// `{lower-case part name: Port}` of a ship from its vehicle definition XML; empty when unreadable.
pub fn vehicle_ports(p4k: &P4k, items: &ItemMaterials, cls: &str) -> HashMap<String, Port> {
    let Some(d) = items.datacore() else { return HashMap::new() };
    let Some(ids) = d.record_index().get(&format!("{}{}", RECORD, cls)) else { return HashMap::new() };
    let Ok(rec) = d.record(ids[0]) else { return HashMap::new() };
    let Ok(data) = d.record_data(&rec, 3) else { return HashMap::new() };
    let data = data.to_json();
    let path = comp(&data, "VehicleComponentParams").and_then(|c| c.get("vehicleDefinition")).and_then(Value::as_str).unwrap_or("");
    if path.is_empty() {
        return HashMap::new();
    }
    let Some(ent) = p4k.find_path(path, ".xml") else { return HashMap::new() };
    p4k.read(ent).ok().and_then(|b| parse_vehicle_xml(&b).ok()).unwrap_or_default()
}

/// `{lower-case port name: Port}` of an item record (SItemPortContainerComponentParams), in port order.
pub fn item_ports(stats: &dyn ItemData, cls: &str) -> Vec<(String, Port)> {
    let mut out: Vec<(String, Port)> = vec![];
    if cls.is_empty() {
        return out;
    }
    let Some(data) = stats.record(cls, 8) else { return out };
    let Some(pc) = comp(&data, "SItemPortContainerComponentParams") else { return out };
    for p in pc.get("Ports").and_then(Value::as_array).into_iter().flatten() {
        let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
        let types: Vec<(String, Vec<String>)> = p
            .get("Types")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|t| t.is_object())
            .map(|t| (t.get("Type").and_then(Value::as_str).unwrap_or("").to_string(), t.get("SubTypes").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()))
            .collect();
        let geti = |k: &str| p.get(k).and_then(Value::as_i64).unwrap_or(0);
        let port = Port::new(s("Name"), s("Flags"), geti("MinSize"), geti("MaxSize"), &types, s("RequiredPortTags"), s("PortTags"));
        let key = s("Name").to_lowercase();
        match out.iter_mut().find(|e| e.0 == key) {
            Some(e) => e.1 = port,
            None => out.push((key, port)),
        }
    }
    out
}

/// The port table the children of `parent_cls` hang on (the ship's vehicle XML when `parent_cls` is the ship itself).
pub fn ports_of(items: &ItemMaterials, stats: &dyn ItemData, p4k: &P4k, ship: &str, parent_cls: &str) -> HashMap<String, Port> {
    if parent_cls == ship {
        vehicle_ports(p4k, items, ship)
    } else {
        item_ports(stats, parent_cls).into_iter().collect()
    }
}

// ---------------------------------------------------------------------------------------------------------------- candidate pool
/// Every entity record with an AttachDef (one pass over the DataCore), in record order.
pub fn build_pool(items: &ItemMaterials) -> Vec<PoolItem> {
    let Some(d) = items.datacore() else { return vec![] };
    let mut names: Vec<(&String, usize)> = d.record_index().iter().filter(|(k, _)| k.starts_with(RECORD)).map(|(k, v)| (k, v[0])).collect();
    names.sort_by_key(|x| x.1);
    names
        .par_iter()
        .filter_map(|(k, id)| {
            let rec = d.record(*id).ok()?;
            let data = d.record_data(&rec, 4).ok()?.to_json();
            for c in data.get("Components")?.as_array()? {
                if c.get("__struct__").and_then(Value::as_str) == Some("SAttachableComponentParams") {
                    let a = c.get("AttachDef").filter(|v| v.is_object());
                    let t = a.and_then(|a| a.get("Type")).and_then(Value::as_str).filter(|t| !t.is_empty())?;
                    let a = a.unwrap();
                    return Some(PoolItem {
                        cls: k[RECORD.len()..].to_string(),
                        atype: t.to_string(),
                        subtype: a.get("SubType").and_then(Value::as_str).unwrap_or("").to_string(),
                        size: a.get("Size").and_then(Value::as_i64),
                        grade: a.get("Grade").and_then(Value::as_i64),
                        tags: tags(a.get("Tags").and_then(Value::as_str).unwrap_or("")),
                        req: tags(a.get("RequiredTags").and_then(Value::as_str).unwrap_or("")),
                    });
                }
            }
            None
        })
        .collect()
}

/// Candidate pool and per-port candidate lists.  `pool()` builds the pool the first time it is asked.
pub struct SwapIndex<'a> {
    items: &'a ItemMaterials,
    pool: OnceLock<(Vec<PoolItem>, HashMap<String, Vec<usize>>, HashMap<String, usize>)>,
}

impl<'a> SwapIndex<'a> {
    pub fn new(items: &'a ItemMaterials) -> Self {
        SwapIndex { items, pool: OnceLock::new() }
    }

    fn built(&self) -> &(Vec<PoolItem>, HashMap<String, Vec<usize>>, HashMap<String, usize>) {
        self.pool.get_or_init(|| {
            let pool = build_pool(self.items);
            let (mut by_type, mut by_cls): (HashMap<String, Vec<usize>>, HashMap<String, usize>) = (HashMap::new(), HashMap::new());
            for (i, it) in pool.iter().enumerate() {
                by_type.entry(it.atype.clone()).or_default().push(i);
                by_cls.insert(it.cls.clone(), i);
            }
            (pool, by_type, by_cls)
        })
    }

    pub fn pool(&self) -> &[PoolItem] {
        &self.built().0
    }

    pub fn info(&self, cls: &str) -> Option<&PoolItem> {
        let b = self.built();
        b.2.get(cls).map(|&i| &b.0[i])
    }

    /// Tags a port gets from its default item's RequiredTags.
    pub fn host_tags(&self, default_cls: &str) -> Tags {
        if default_cls.is_empty() {
            return Tags::new();
        }
        self.info(default_cls).map(|i| i.req.clone()).unwrap_or_default()
    }

    /// Pool entries that fit `port` (class name order).
    pub fn candidates(&self, port: &Port, host: &Tags) -> Vec<&PoolItem> {
        let b = self.built();
        let mut found: HashMap<&str, &PoolItem> = HashMap::new();
        for (t, _) in &port.types {
            for &i in b.1.get(t).into_iter().flatten() {
                let it = &b.0[i];
                if compatible(port, it, host) {
                    found.insert(&it.cls, it);
                }
            }
        }
        let mut v: Vec<&PoolItem> = found.into_values().collect();
        v.sort_by(|a, b| a.cls.cmp(&b.cls));
        v
    }

    /// None when class `cls` fits `port`, else the first failing check ('unknown' = not in the pool, 'notypes' = port lists none).
    pub fn why(&self, port: &Port, cls: &str, host: &Tags) -> Option<&'static str> {
        let Some(it) = self.info(cls) else { return Some("unknown") };
        if port.types.is_empty() {
            Some("notypes")
        } else {
            mismatch(port, it, host)
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------- swap sets
/// `{port path: class}` from `path:class;path:class` (the URL hash form).  A path is the port names from the ship down joined by '/'.
pub fn parse_swaps(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for part in text.split(';') {
        if let Some((path, cls)) = part.rsplit_once(':') {
            if !path.is_empty() && !cls.is_empty() {
                out.insert(path.to_lowercase(), cls.to_string());
            }
        }
    }
    out
}

pub fn format_swaps(swaps: &std::collections::BTreeMap<String, String>) -> String {
    swaps.iter().map(|(p, c)| format!("{}:{}", p, c)).collect::<Vec<_>>().join(";")
}

fn node_port(n: &Value) -> String {
    n.get("port").and_then(Value::as_str).unwrap_or("").to_lowercase()
}

fn node_cls(n: &Value) -> &str {
    n.get("cls").and_then(Value::as_str).unwrap_or("")
}

/// Adds `path` and `slot` ({swappable, min, max, types}) to every node of a loadout tree (itemstats `tree` JSON), in place.
pub fn annotate(nodes: &mut [Value], items: &ItemMaterials, stats: &dyn ItemData, p4k: &P4k, ship: &str, parent_cls: Option<&str>, path: &str) {
    let parent_cls = parent_cls.filter(|p| !p.is_empty()).unwrap_or(ship);
    let table = ports_of(items, stats, p4k, ship, parent_cls);
    for n in nodes.iter_mut() {
        let here = if path.is_empty() { node_port(n) } else { format!("{}/{}", path, node_port(n)) };
        n["path"] = json!(here);
        n["slot"] = match table.get(&node_port(n)) {
            Some(port) => {
                let mut d = port.describe();
                d["swappable"] = json!(port.swappable());
                d
            }
            None => Value::Null,
        };
        let cls = node_cls(n).to_string();
        if let Some(ch) = n.get_mut("children").and_then(Value::as_array_mut) {
            annotate(ch, items, stats, p4k, ship, Some(if cls.is_empty() { parent_cls } else { &cls }), &here);
        }
    }
}

/// Class of the item whose port table holds `path` (the ship for a top-level port).
pub fn parent_of(path: &str, tree: &[Value], ship: &str) -> String {
    let mut cur: &[Value] = tree;
    let mut cls = ship.to_string();
    let segs: Vec<&str> = path.split('/').collect();
    for seg in &segs[..segs.len() - 1] {
        let Some(n) = cur.iter().find(|x| node_port(x) == *seg) else { return cls };
        let c = node_cls(n);
        if !c.is_empty() {
            cls = c.to_string();
        }
        cur = n.get("children").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    }
    cls
}

/// (Port, default item class) of the port at `path` of a loadout tree (annotated or not), or None when unknown.  The tree must be the DEFAULT
/// one: its node at `path` names the default item whose RequiredTags are the port's host tags.
pub fn port_at(items: &ItemMaterials, stats: &dyn ItemData, p4k: &P4k, tree: &[Value], ship: &str, path: &str) -> Option<(Port, String)> {
    let path = path.to_lowercase();
    let mut cur: &[Value] = tree;
    let mut node: Option<&Value> = None;
    for seg in path.split('/') {
        node = Some(cur.iter().find(|x| node_port(x) == seg)?);
        cur = node?.get("children").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    }
    let port = ports_of(items, stats, p4k, ship, &parent_of(&path, tree, ship)).remove(path.rsplit('/').next().unwrap())?;
    Some((port, node_cls(node?).to_string()))
}

/// Sets `swapped` / `default` (the class the ship ships with) on the nodes of a tree built with `swaps` (in place).
pub fn mark_swapped(nodes: &mut [Value], swaps: &std::collections::BTreeMap<String, String>, default_tree: &[Value]) {
    for n in nodes.iter_mut() {
        let path = n.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if swaps.contains_key(&path) {
            let mut node: Option<&[Value]> = Some(default_tree);
            let mut dflt: Option<String> = None;
            for seg in path.split('/') {
                let Some(list) = node else { break };
                match list.iter().find(|x| node_port(x) == seg) {
                    None => {
                        node = None;
                    }
                    Some(x) => {
                        dflt = Some(node_cls(x).to_string());
                        node = Some(x.get("children").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]));
                    }
                }
            }
            n["swapped"] = json!(true);
            n["default"] = json!(dflt);
        }
        if let Some(ch) = n.get_mut("children").and_then(Value::as_array_mut) {
            mark_swapped(ch, swaps, default_tree);
        }
    }
}

/// The entries of `swaps` that name a swappable port of the DEFAULT tree and a class that fits it (others are dropped).
pub fn clean(items: &ItemMaterials, stats: &dyn ItemData, p4k: &P4k, index: &SwapIndex, ship: &str, default_tree: &[Value], swaps: &std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (path, cls) in swaps {
        if let Some((port, dflt)) = port_at(items, stats, p4k, default_tree, ship, path) {
            if port.swappable() && *cls != dflt && index.why(&port, cls, &index.host_tags(&dflt)).is_none() {
                out.insert(path.to_lowercase(), cls.clone());
            }
        }
    }
    out
}

/// The `accept` callback of ships.loadout_tree: may item `cls` stay on the child port `path` of a swapped item.
pub fn accept(stats: &dyn ItemData, index: &SwapIndex, swaps: &std::collections::BTreeMap<String, String>, path: &str, cls: &str) -> bool {
    let (parent, name) = path.rsplit_once('/').map_or(("", path), |(a, b)| (a, b));
    if cls.is_empty() || !swaps.contains_key(parent) {
        return true;
    }
    let ports = item_ports(stats, &swaps[parent]);
    match ports.iter().find(|(k, _)| k == name) {
        None => true,
        Some((_, p)) => p.types.is_empty() || index.why(p, cls, &index.host_tags(cls)).is_none(),
    }
}

/// {port, default, items: [{cls, name, size, grade, subtype, manufacturer, type}]} for the swappable port at `path` or None.
pub fn candidates(items: &ItemMaterials, stats: &dyn ItemData, p4k: &P4k, index: &SwapIndex, default_tree: &[Value], ship: &str, path: &str) -> Option<Value> {
    let (port, dflt) = port_at(items, stats, p4k, default_tree, ship, path)?;
    if !port.swappable() {
        return None;
    }
    let mut rows: Vec<(Option<i64>, i64, String, Value)> = vec![];
    for it in index.candidates(&port, &index.host_tags(&dflt)) {
        let ident = stats.identity(&it.cls);
        rows.push((
            it.size,
            it.grade.unwrap_or(0),
            ident.name.clone(),
            json!({"cls": it.cls, "name": ident.name, "size": it.size, "grade": it.grade, "subtype": it.subtype, "manufacturer": ident.manufacturer, "type": it.atype}),
        ));
    }
    rows.sort_by(|a, b| a.0.unwrap_or(99).cmp(&b.0.unwrap_or(99)).then((-a.1).cmp(&(-b.1))).then(a.2.cmp(&b.2)));
    Some(json!({"port": port.describe(), "default": dflt, "items": rows.into_iter().map(|r| r.3).collect::<Vec<_>>()}))
}

/// One placed item of a flattened loadout: (geometry path, 3x4 matrix, meta {cls, port}).
pub type LoadoutItem = (String, [f64; 12], Value);

/// `{remove: [{g, m}], add: [{g, m, cls, port}]}`: default loadout items missing from the swapped loadout and the other way round (items match by
/// geometry path and placement to 1 mm).
pub fn diff(base: &[LoadoutItem], now: &[LoadoutItem]) -> Value {
    fn key(g: &str, m: &[f64; 12]) -> (String, Vec<i64>) {
        // round(v, 3) then tuple equality: compare the rounded decimals
        (g.replace('\\', "/").to_lowercase().trim_start_matches('/').to_string(), m.iter().map(|v| (crate::character::round_nd(*v, 3) * 1000.0).round() as i64).collect())
    }
    let mut now_count: HashMap<(String, Vec<i64>), usize> = HashMap::new();
    for (g, m, _) in now {
        *now_count.entry(key(g, m)).or_insert(0) += 1;
    }
    let mut base_count: HashMap<(String, Vec<i64>), usize> = HashMap::new();
    for (g, m, _) in base {
        *base_count.entry(key(g, m)).or_insert(0) += 1;
    }
    let (mut remove, mut seen) = (vec![], HashMap::new());
    for (g, m, _) in base {
        let k = key(g, m);
        let s = seen.entry(k.clone()).or_insert(0usize);
        *s += 1;
        if *s > now_count.get(&k).copied().unwrap_or(0) {
            remove.push(json!({"g": g, "m": m.to_vec()}));
        }
    }
    let (mut add, mut seen) = (vec![], HashMap::new());
    for (g, m, meta) in now {
        let k = key(g, m);
        let s = seen.entry(k.clone()).or_insert(0usize);
        *s += 1;
        if *s > base_count.get(&k).copied().unwrap_or(0) {
            add.push(json!({"g": g, "m": m.to_vec(), "cls": meta.get("cls"), "port": meta.get("port")}));
        }
    }
    json!({"remove": remove, "add": add})
}

// ---------------------------------------------------------------------------------------------------------------- ShipSwaps
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::itemstats::ItemStats;
use crate::ships::{Ships, TreeNode};

/// A loadout tree (ships.rs `TreeNode`) as the JSON dict `ItemStats::tree` consumes (`geo`, `m`, `helper` only when set).
pub fn tree_json(nodes: &[TreeNode]) -> Value {
    Value::Array(
        nodes
            .iter()
            .map(|n| {
                let mut o = json!({"port": n.port, "cls": n.cls, "children": tree_json(&n.children), "sel": n.sel});
                if let Some(g) = &n.geo {
                    o["geo"] = json!(g);
                }
                if let Some(m) = &n.m {
                    o["m"] = json!(m);
                }
                if let Some(h) = &n.helper {
                    o["helper"] = json!(h);
                }
                o
            })
            .collect(),
    )
}

type SwapKey = (String, Vec<(String, String)>);

/// Everything the server needs per ship: validated swap sets, the loadout tree with a swap set applied, candidate lists.
/// The caches live as long as the object.
pub struct ShipSwaps<'a> {
    pub items: &'a ItemMaterials,
    pub stats: &'a ItemStats<'a>,
    pub p4k: &'a P4k,
    pub index: &'a SwapIndex<'a>,
    pub ships: &'a Ships<'a>,
    default: Mutex<HashMap<String, Arc<Vec<Value>>>>,
    trees: Mutex<HashMap<SwapKey, Arc<Vec<Value>>>>,
    flat: Mutex<HashMap<SwapKey, Arc<Vec<LoadoutItem>>>>,
}

fn key_of(ship: &str, swaps: &BTreeMap<String, String>) -> SwapKey {
    (ship.to_string(), swaps.iter().map(|(a, b)| (a.clone(), b.clone())).collect())
}

impl<'a> ShipSwaps<'a> {
    pub fn new(items: &'a ItemMaterials, stats: &'a ItemStats<'a>, p4k: &'a P4k, index: &'a SwapIndex<'a>, ships: &'a Ships<'a>) -> Self {
        ShipSwaps { items, stats, p4k, index, ships, default: Mutex::new(HashMap::new()), trees: Mutex::new(HashMap::new()), flat: Mutex::new(HashMap::new()) }
    }

    /// Annotated (path / slot) default loadout tree with item identities.
    pub fn default_tree(&self, ship: &str) -> Arc<Vec<Value>> {
        if let Some(t) = self.default.lock().unwrap().get(ship) {
            return t.clone();
        }
        let mut unplaced = 0;
        let raw = self.ships.loadout_tree(ship, &mut unplaced, None, None);
        let mut tree = match self.stats.tree(&tree_json(&raw)) {
            Value::Array(a) => a,
            _ => vec![],
        };
        annotate(&mut tree, self.items, self.stats, self.p4k, ship, None, "");
        let a = Arc::new(tree);
        self.default.lock().unwrap().insert(ship.to_string(), a.clone());
        a
    }

    /// The entries of `swaps` that name a swappable port of the DEFAULT tree and a class that fits it (others are dropped).
    pub fn clean(&self, ship: &str, swaps: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let tree = self.default_tree(ship);
        clean(self.items, self.stats, self.p4k, self.index, ship, &tree, swaps)
    }

    /// Loadout tree (identity form + path, slot, swapped, default) with `swaps` (already cleaned) applied.
    pub fn tree(&self, ship: &str, swaps: &BTreeMap<String, String>) -> Arc<Vec<Value>> {
        if swaps.is_empty() {
            return self.default_tree(ship);
        }
        let key = key_of(ship, swaps);
        if let Some(t) = self.trees.lock().unwrap().get(&key) {
            return t.clone();
        }
        let hm: HashMap<String, String> = swaps.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
        let acc = |p: &str, c: &str| accept(self.stats, self.index, swaps, p, c);
        let mut unplaced = 0;
        let raw = self.ships.loadout_tree(ship, &mut unplaced, Some(&hm), Some(&acc));
        let mut tree = match self.stats.tree(&tree_json(&raw)) {
            Value::Array(a) => a,
            _ => vec![],
        };
        annotate(&mut tree, self.items, self.stats, self.p4k, ship, None, "");
        mark_swapped(&mut tree, swaps, &self.default_tree(ship));
        let a = Arc::new(tree);
        self.trees.lock().unwrap().insert(key, a.clone());
        a
    }

    /// `Ships::loadout_items` with `swaps` applied, as (geometry path, matrix, {cls, port}).
    pub fn items_for(&self, ship: &str, swaps: &BTreeMap<String, String>) -> Arc<Vec<LoadoutItem>> {
        let key = key_of(ship, swaps);
        if let Some(t) = self.flat.lock().unwrap().get(&key) {
            return t.clone();
        }
        let hm: HashMap<String, String> = swaps.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
        let acc = |p: &str, c: &str| accept(self.stats, self.index, swaps, p, c);
        let mut unplaced = 0;
        let placed = self.ships.loadout_items(ship, &mut unplaced, if swaps.is_empty() { None } else { Some(&hm) }, Some(&acc));
        let out: Vec<LoadoutItem> = placed.into_iter().map(|p| (p.geo, p.matrix, json!({"cls": p.cls, "port": p.port}))).collect();
        let a = Arc::new(out);
        self.flat.lock().unwrap().insert(key, a.clone());
        a
    }

    /// {port, default, items} for the swappable port at `path` or None.
    pub fn candidates(&self, ship: &str, path: &str) -> Option<Value> {
        let tree = self.default_tree(ship);
        candidates(self.items, self.stats, self.p4k, self.index, &tree, ship, path)
    }

    /// {remove: [{g, m}], add: [{g, m, cls, port}]}: default loadout items missing from the swapped loadout and the other way round.
    pub fn diff(&self, ship: &str, swaps: &BTreeMap<String, String>) -> Value {
        let base = self.items_for(ship, &BTreeMap::new());
        let now = self.items_for(ship, swaps);
        diff(&base, &now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml(attrs: &str) -> Vec<u8> {
        format!(r#"<Vehicle><Parts><Part name="Hardpoint_Power_Plant" class="ItemPort"><ItemPort {} flags="$editable"><Types><Type type="PowerPlant" subtypes=""/></Types></ItemPort></Part></Parts></Vehicle>"#, attrs).into_bytes()
    }

    fn item(size: i64) -> PoolItem {
        PoolItem { cls: format!("POWR_S0{}", size), atype: "PowerPlant".into(), subtype: "Power".into(), size: Some(size), grade: Some(1), tags: Tags::new(), req: Tags::new() }
    }

    #[test]
    fn size_attributes_are_read_in_either_spelling() {
        for attrs in [r#"minSize="1" maxSize="1""#, r#"minsize="1" maxsize="1""#, r#"minSize="1" maxsize="1""#] {
            let ports = parse_vehicle_xml(&xml(attrs)).unwrap();
            let p = &ports["hardpoint_power_plant"];
            assert_eq!((p.min, p.max), (1, 1), "{}", attrs);
            assert!(p.swappable());
            assert!(compatible(p, &item(1), &Tags::new()));
            assert!(!compatible(p, &item(0), &Tags::new()), "an S0 item must not fit an S1 port ({})", attrs);
        }
    }

    #[test]
    fn missing_size_attributes_stay_zero() {
        let p = &parse_vehicle_xml(&xml("")).unwrap()["hardpoint_power_plant"];
        assert_eq!((p.min, p.max), (0, 0));
    }
}
