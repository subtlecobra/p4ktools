//! Armor and clothing: the index of every wearable item of Game2.dcb, their colour variants (skins) and set families.
//! Evidence notes (EMPIRICAL on the live archive):
//!
//! * Wearables are records under `entities/scitem/characters/` whose `AttachDef.Type` is `Char_Armor_<Slot>` or `Char_Clothing_<Slot>`,
//!   `Char_Accessory_Eyes` or `Char_Flair`.  The slot is the type's last word; the character customiser records are never indexed.
//! * The worn model is the `Male` (or `Female`) `SubGeometry` node: a `.skin` (bind pose of the player skeleton), a rigid `.cga` (backpacks)
//!   or a `.cdf`.  The main node and `heldEntity` are the carry box.  An empty material on a node inherits a sibling's (INFERRED).
//! * Colour variants are separate classes with the same model and stats and another palette / material: grouped by (type, worn model,
//!   `gearstats::signature_key`); the shortest class name of a group is the base.
//! * A set shares its class stem once the slot word and the trailing `_<variant>_<colour>` are removed; only armor has sets.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use p4k::P4k;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::dynv::{arr, g, s, struct_of, truthy};
use crate::gearstats;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::{pretty, ItemStats, RECORD};
use crate::weapons::{clean_text, ordered_names, Weapons};

/// slot id -> (label, kind); the tab order of the index is the order here.
pub const SLOTS: [(&str, &str, &str); 15] = [
    ("helmet", "Helmets", "armor"),
    ("torso", "Core", "armor"),
    ("arms", "Arms", "armor"),
    ("legs", "Legs", "armor"),
    ("backpack", "Backpacks", "armor"),
    ("undersuit", "Undersuits", "armor"),
    ("torso_0", "Shirts", "clothing"),
    ("torso_1", "Jackets and outfits", "clothing"),
    ("torso_2", "Belts and uniform pieces", "clothing"),
    ("pants", "Pants", "clothing"),
    ("feet", "Footwear", "clothing"),
    ("hands", "Gloves", "clothing"),
    ("hat", "Hats and masks", "clothing"),
    ("eyes", "Eyewear", "clothing"),
    ("flair", "Neck", "clothing"),
];
const EXCLUDED_SUBTYPES: [&str; 1] = ["Medical"];

pub fn slot_label(slot: &str) -> Option<&'static str> {
    SLOTS.iter().find(|x| x.0 == slot).map(|x| x.1)
}

fn re(pat: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn carry_box() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(r"(?i)/(crate|box_cardboard|box_|shopdisplay)[^/]*", &R)
}

fn skip_name() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(r"(?i)(^|_)(template|test|dev|placeholder)(_|$)", &R)
}

/// (kind, slot id) of an `AttachDef.Type`, or None when the type is not a wearable; `Medical` sub-types are None.
pub fn classify(att_type: &str, att_subtype: &str) -> Option<(&'static str, String)> {
    if att_type.is_empty() || EXCLUDED_SUBTYPES.contains(&att_subtype) {
        return None;
    }
    let (kind, slot) = if let Some(r) = att_type.strip_prefix("Char_Armor_") {
        ("armor", r.to_lowercase())
    } else if let Some(r) = att_type.strip_prefix("Char_Clothing_") {
        let l = r.to_lowercase();
        ("clothing", if l == "legs" { "pants".to_string() } else { l })
    } else if att_type == "Char_Accessory_Eyes" {
        ("clothing", "eyes".to_string())
    } else if att_type == "Char_Flair" {
        ("clothing", "flair".to_string())
    } else {
        return None;
    };
    if slot_label(&slot).is_some() {
        Some((kind, slot))
    } else {
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct WornNode {
    pub path: String,
    pub material: String,
    pub palette_ref: Value,
}

fn node_info(n: &Value) -> WornNode {
    let gm = g(n, "Geometry");
    WornNode {
        path: s(g(g(gm, "Geometry"), "path")).replace('\\', "/"),
        material: s(g(g(gm, "Material"), "path")).replace('\\', "/"),
        palette_ref: g(gm, "Palette").clone(),
    }
}

/// {lower-case tag: node} of the `SubGeometry` nodes of an `SGeometryNodeParams` dict plus "main" for its own node, in that order (a node's
/// tags may hold several words: each is a key, first node wins).
pub fn worn_nodes(geometry: &Value) -> Vec<(String, WornNode)> {
    let mut out = vec![("main".to_string(), node_info(geometry))];
    for sub in arr(g(geometry, "SubGeometry")) {
        if truthy(sub) {
            for t in s(g(sub, "Tags")).to_lowercase().split_whitespace() {
                if !out.iter().any(|(k, _)| k == t) {
                    out.push((t.to_string(), node_info(sub)));
                }
            }
        }
    }
    out
}

/// (path, material, palette_ref, tag) of the worn model for `sex`: the node of that tag, else the other sex's node, else the main node
/// when its path is not a carry box; empty strings when there is none.  The material falls back to a sibling node's, then "".
pub fn worn_model(nodes: &[(String, WornNode)], sex: &str) -> (String, String, Value, String) {
    let other = if sex == "male" { "female" } else { "male" };
    for tag in [sex, other, "main"] {
        let Some((_, n)) = nodes.iter().find(|(k, _)| k == tag) else { continue };
        if !n.path.is_empty() && !(tag == "main" && carry_box().is_match(&n.path)) {
            let mat = if !n.material.is_empty() {
                n.material.clone()
            } else {
                let a = nodes.iter().find(|(k, x)| !x.material.is_empty() && k != "heldentity" && x.path.to_lowercase() == n.path.to_lowercase()).map(|(_, x)| x.material.clone());
                match a {
                    Some(m) if !m.is_empty() => m,
                    _ => nodes
                        .iter()
                        .find(|(k, x)| !x.material.is_empty() && ["female", "male", "hanging"].contains(&k.as_str()))
                        .map(|(_, x)| x.material.clone())
                        .unwrap_or_default(),
                }
            };
            return (n.path.clone(), mat, n.palette_ref.clone(), tag.to_string());
        }
    }
    (String::new(), String::new(), Value::Null, String::new())
}

/// Set family key of a class name: the name without its trailing `_<variant>_<colour>` and without the slot word.  Names that do not follow
/// the pattern keep the whole lower-case class name.
pub fn stem_of(cls: &str, slot_words: &[String]) -> String {
    static A: OnceLock<Regex> = OnceLock::new();
    static B: OnceLock<Regex> = OnceLock::new();
    static C: OnceLock<Regex> = OnceLock::new();
    let low = cls.to_lowercase();
    let parts: Vec<String> = if let Some(m) = re(r"^(.*?)_(\d+)_(\d+)_[^_]+$", &A).captures(&low).or_else(|| re(r"^(.*?)_(\d+)_\d+$", &B).captures(&low)) {
        let mut p: Vec<String> = m[1].split('_').map(str::to_string).collect();
        p.push(m[2].to_string());
        p
    } else {
        let base = re(r"^(.*?_\d+)(?:_[^_]+)?$", &C).captures(&low).map(|m| m[1].to_string()).unwrap_or_else(|| low.clone());
        base.split('_').map(str::to_string).collect()
    };
    parts.into_iter().filter(|p| !slot_words.contains(p)).collect::<Vec<_>>().join("_")
}

/// {slot id: words} the stem rule drops: for each slot the word that precedes the first number in most of its class names, when it is shared
/// by at least a quarter of the slot's classes (EMPIRICAL).
pub fn slot_words_by_data(classes_by_slot: &BTreeMap<String, Vec<String>>) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    for (slot, classes) in classes_by_slot {
        let mut count: HashMap<String, usize> = HashMap::new();
        for c in classes {
            let parts: Vec<String> = c.to_lowercase().split('_').map(str::to_string).collect();
            for (i, p) in parts.iter().enumerate() {
                if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
                    if i > 0 {
                        *count.entry(parts[i - 1].clone()).or_insert(0) += 1;
                    }
                    break;
                }
            }
        }
        let need = classes.len().max(1);
        out.insert(slot.clone(), count.into_iter().filter(|(_, n)| n * 4 >= need).map(|(w, _)| w).collect());
    }
    out
}

fn exists(p4k: &P4k, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let dot = path.rfind('.').unwrap_or(0);
    let ext = path[dot..].to_lowercase();
    if ext == ".skin" {
        return p4k.find_path(&format!("{}.skinm", &path[..dot]), ".skinm").is_some();
    }
    p4k.find_path(path, &ext).is_some()
}

// ---------------------------------------------------------------------------------------------------------------- the index
/// Entries `{cls, kind, slot, type, subtype, size, grade, name, manufacturer, named, short, model, model_ok, material, palette, hold, tags,
/// file, facts}` of every wearable.
pub fn scan(items: &ItemMaterials, stats: &ItemStats, weapons: &Weapons, p4k: &P4k) -> Vec<Value> {
    let Some(d) = items.datacore() else { return vec![] };
    let names: Vec<(String, usize)> = ordered_names(items, RECORD)
        .into_iter()
        .filter(|(_, i)| d.record_file(*i).map(|f| f.to_lowercase().contains("/scitem/characters/")).unwrap_or(false))
        .collect();
    // The records are independent and decoding them dominates (malloc-bound): one task per record, results kept in record order.
    use rayon::prelude::*;
    let out: Vec<Value> = names.par_iter().filter_map(|(k, i)| {
        let i = *i;
        let cls = &k[RECORD.len()..];
        let Ok(rec) = d.record(i) else { return None };
        let Ok(data) = d.record_data(&rec, 8) else { return None };
        let data = data.to_json();
        let comps = arr(g(&data, "Components"));
        let comp = |n: &str| comps.iter().find(|c| truthy(c) && struct_of(c) == n);
        let null = Value::Null;
        let att = g(comp("SAttachableComponentParams").unwrap_or(&null), "AttachDef");
        if !truthy(att) || !truthy(g(att, "Type")) {
            return None;
        }
        let Some(cl) = classify(s(g(att, "Type")), s(g(att, "SubType"))) else { return None };
        let ident = stats.identity(cls);
        let loc = g(att, "Localization");
        let geo = g(comp("SGeometryResourceParams").unwrap_or(&null), "Geometry");
        let nodes = if truthy(geo) { worn_nodes(geo) } else { vec![] };
        let (path, mat, pal, _tag) = if nodes.is_empty() { (String::new(), String::new(), Value::Null, String::new()) } else { worn_model(&nodes, "male") };
        let hold_node = nodes.iter().find(|(k, _)| k == "heldentity").or_else(|| nodes.iter().find(|(k, _)| k == "main"));
        let hold = hold_node.map(|(_, n)| n.path.clone()).unwrap_or_default();
        let short_key = s(g(loc, "ShortName")).to_string();
        let facts = gearstats::facts(stats, &data);
        let name = ident["name"].as_str().unwrap_or("").to_string();
        Some(json!({
            "cls": cls, "kind": cl.0, "slot": cl.1, "type": g(att, "Type"), "subtype": if truthy(g(att, "SubType")) { g(att, "SubType").clone() } else { json!("") },
            "size": g(att, "Size"), "grade": g(att, "Grade"), "name": name, "manufacturer": clean_text(ident["manufacturer"].as_str().unwrap_or("")),
            "named": name != pretty(cls),
            "short": if short_key != "@LOC_EMPTY" { clean_text(&stats.localise_str(&short_key).unwrap_or_default()) } else { String::new() },
            "model": path, "model_ok": exists(p4k, &path), "material": mat, "palette": weapons.palette_key(&pal),
            "hold": hold, "tags": if truthy(g(att, "Tags")) { g(att, "Tags").clone() } else { json!("") }, "file": rec.file, "facts": Value::Object(facts),
        }))
    }).collect();
    out
}

/// The leading words all `names` share, "" for none or one name.
pub fn common_words(names: &[String]) -> String {
    if names.len() < 2 {
        return String::new();
    }
    let words: Vec<Vec<&str>> = names.iter().map(|n| n.split_whitespace().collect()).collect();
    let n = words.iter().map(Vec::len).min().unwrap_or(0);
    let mut out = vec![];
    for i in 0..n {
        let w = words[0][i];
        if words.iter().any(|x| x[i] != w) {
            break;
        }
        out.push(w);
    }
    out.join(" ")
}

fn mode_of(vals: &[String]) -> String {
    // the most common value; on a tie the smallest string wins, so the result does not depend on hash order
    let mut count: BTreeMap<&String, usize> = BTreeMap::new();
    for v in vals {
        *count.entry(v).or_insert(0) += 1;
    }
    let mut best: Option<(&String, usize)> = None;
    for (v, n) in count {
        if best.map_or(true, |(_, bn)| n > bn) {
            best = Some((v, n));
        }
    }
    best.map(|b| b.0.clone()).unwrap_or_default()
}

/// Sets `base` (class of the item an entry is a colour variant of, null for a base), `skins` (number of variants of a base), `set` / `set_name`
/// on every entry (in place).  Group = (type, lower-case model path, signature_key); base = the named entry with the shortest class name.
pub fn link_variants(entries: &mut [Value]) {
    let n = entries.len();
    let mut groups: Vec<((String, String, String), Vec<usize>)> = vec![];
    let mut gidx: HashMap<(String, String, String), usize> = HashMap::new();
    for (i, e) in entries.iter_mut().enumerate() {
        e["base"] = Value::Null;
        e["skins"] = json!(0);
        e["set_name"] = json!("");
        if truthy(g(e, "model")) {
            let facts = g(e, "facts").as_object().cloned().unwrap_or_default();
            let key = (s(g(e, "type")).to_string(), s(g(e, "model")).to_lowercase(), gearstats::signature_key(&facts));
            let at = *gidx.entry(key.clone()).or_insert_with(|| {
                groups.push((key, vec![]));
                groups.len() - 1
            });
            groups[at].1.push(i);
        }
    }
    for (_, grp) in groups.iter_mut() {
        grp.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            let ka = (g(ea, "named").as_bool() != Some(true), s(g(ea, "cls")).chars().count(), s(g(ea, "cls")).to_string());
            let kb = (g(eb, "named").as_bool() != Some(true), s(g(eb, "cls")).chars().count(), s(g(eb, "cls")).to_string());
            ka.cmp(&kb)
        });
        let base_cls = s(g(&entries[grp[0]], "cls")).to_string();
        for &i in &grp[1..] {
            entries[i]["base"] = json!(base_cls);
            let b = grp[0];
            let k = g(&entries[b], "skins").as_i64().unwrap_or(0) + 1;
            entries[b]["skins"] = json!(k);
        }
    }
    let mut by_slot: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in entries.iter() {
        by_slot.entry(s(g(e, "slot")).to_string()).or_default().push(s(g(e, "cls")).to_string());
    }
    let words = slot_words_by_data(&by_slot);
    let mut fam: Vec<(String, Vec<usize>)> = vec![];
    let mut fidx: HashMap<String, usize> = HashMap::new();
    for i in 0..n {
        let armor = s(g(&entries[i], "kind")) == "armor";
        let set = if armor { Some(stem_of(s(g(&entries[i], "cls")), words.get(s(g(&entries[i], "slot"))).map(Vec::as_slice).unwrap_or(&[]))) } else { None };
        match set {
            Some(k) if !k.is_empty() => {
                entries[i]["set"] = json!(k);
                let at = *fidx.entry(k.clone()).or_insert_with(|| {
                    fam.push((k, vec![]));
                    fam.len() - 1
                });
                fam[at].1.push(i);
            }
            _ => entries[i]["set"] = if armor { json!("") } else { Value::Null },
        }
    }
    for (_, members) in &fam {
        // a record without a manufacturer takes its set's (most common one among the members that have it)
        let known: Vec<String> = members.iter().map(|&m| s(g(&entries[m], "manufacturer")).to_string()).filter(|x| !x.is_empty()).collect();
        if !known.is_empty() {
            let top = mode_of(&known);
            for &m in members {
                if s(g(&entries[m], "manufacturer")).is_empty() {
                    entries[m]["manufacturer"] = json!(top);
                    entries[m]["mfr_inferred"] = json!(true);
                }
            }
        }
    }
    for (key, members) in &fam {
        let slots: std::collections::BTreeSet<String> = members.iter().filter(|&&m| g(&entries[m], "base").is_null()).map(|&m| s(g(&entries[m], "slot")).to_string()).collect();
        let names: Vec<String> = members.iter().map(|&m| s(g(&entries[m], "short")).to_string()).filter(|x| !x.is_empty()).collect();
        let name = if !names.is_empty() {
            mode_of(&names)
        } else {
            common_words(&members.iter().filter(|&&m| g(&entries[m], "named").as_bool() == Some(true)).map(|&m| s(g(&entries[m], "name")).to_string()).collect::<Vec<_>>())
        };
        for &m in members {
            if slots.len() < 2 {
                entries[m]["set"] = Value::Null;
                entries[m]["set_name"] = json!("");
            } else {
                entries[m]["set_name"] = json!(if name.is_empty() { key.replace('_', " ") } else { name.clone() });
            }
        }
    }
}

/// True for an entry the index shows: a base item with a localised name and a worn model in the archive that is not a template / test /
/// placeholder.
pub fn is_listed(e: &Value) -> bool {
    g(e, "base").is_null()
        && truthy(g(e, "model"))
        && g(e, "model_ok").as_bool().unwrap_or(true)
        && g(e, "named").as_bool() == Some(true)
        && !skip_name().is_match(s(g(e, "cls")))
}

/// The index with lookups (no background thread, no cache file).
#[derive(Default)]
pub struct GearList {
    pub data: Vec<Value>,
    by_cls: HashMap<String, usize>,
}

impl GearList {
    pub fn new(data: Vec<Value>) -> Self {
        let by_cls = data.iter().enumerate().map(|(i, e)| (s(g(e, "cls")).to_string(), i)).collect();
        GearList { data, by_cls }
    }

    /// scan + link_variants.
    pub fn build(items: &ItemMaterials, stats: &ItemStats, weapons: &Weapons, p4k: &P4k) -> Self {
        let mut data = scan(items, stats, weapons, p4k);
        link_variants(&mut data);
        Self::new(data)
    }

    pub fn get(&self, cls: &str) -> Option<&Value> {
        self.by_cls.get(cls).map(|&i| &self.data[i])
    }

    pub fn listed(&self) -> Vec<&Value> {
        self.data.iter().filter(|e| is_listed(e)).collect()
    }

    pub fn variants(&self, base: &str) -> Vec<&Value> {
        self.data.iter().filter(|e| s(g(e, "base")) == base && !g(e, "base").is_null()).collect()
    }

    pub fn family(&self, key: &str) -> Vec<&Value> {
        self.data.iter().filter(|e| !key.is_empty() && g(e, "set").as_str() == Some(key) && g(e, "base").is_null()).collect()
    }
}

// ---------------------------------------------------------------------------------------------------------------- skins
/// `{model, material, palette}` of the worn model of `cls` (the node rule of `worn_model`) or None.
pub fn look(items: &ItemMaterials, stats: &ItemStats, weapons: &Weapons, p4k: &P4k, cls: &str, sex: &str, convention: bool) -> Option<Value> {
    let data = stats.record(cls, 9)?;
    let mut geo = None;
    for c in arr(g(&data, "Components")) {
        if truthy(c) && struct_of(c) == "SGeometryResourceParams" {
            geo = Some(g(c, "Geometry"));
        }
    }
    let geo = geo.filter(|x| truthy(x))?;
    let (path, mut mat, pal, _t) = worn_model(&worn_nodes(geo), sex);
    if mat.is_empty() && convention {
        mat = convention_material(p4k, &path, cls);
    }
    let _ = items;
    Some(json!({"model": path, "material": if mat.is_empty() { Value::Null } else { json!(mat) }, "palette": weapons.palette_key(&pal)}))
}

/// Material file (path with extension, "" = none) of a `.skin` whose record names no material, by the naming of the archive.
///
/// EMPIRICAL: 105 of the 864 listed wearables have a worn `.skin` and no material in any node of the record; for
/// 100 of them the file sits next to the model and is named after the item class or after the model plus the variant digits.  A `.skin` carries
/// no material name itself.  INFERRED: the engine finds the material the same way; the order tried is the model's own name, the class name,
/// model + the last two digit groups of the class, model + the last one.
pub fn convention_material(p4k: &P4k, path: &str, cls: &str) -> String {
    if path.is_empty() || !path.to_lowercase().ends_with(".skin") {
        return String::new();
    }
    let (d, f) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let stem = &f[..f.len() - 5];
    let pre = if f.chars().nth(1) == Some('_') { &f[..2] } else { "" };
    let lc = cls.to_lowercase();
    let toks: Vec<&str> = lc.split('_').collect();
    let nums: Vec<&str> = toks[toks.len().saturating_sub(2)..].iter().copied().filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())).collect();
    let mut cands = vec![stem.to_string(), format!("{}{}", pre, cls)];
    if nums.len() == 2 {
        cands.push(format!("{}_{}_{}", stem, nums[0], nums[1]));
        cands.push(format!("{}_{}", stem, nums[0]));
    }
    for c in cands {
        let rel = format!("{}/{}.mtl", d, c);
        if p4k.find_path(&rel, ".mtl").is_some() {
            return rel;
        }
    }
    String::new()
}

/// Display name of a colour variant: the words of its localised name the base's name lacks, else the part of the class name after the common
/// prefix, prettified.
pub fn skin_name(base_name: &str, name: &str, cls: &str, base_cls: &str) -> String {
    let bw: std::collections::HashSet<&str> = base_name.split_whitespace().collect();
    let extra: Vec<&str> = name.split_whitespace().filter(|w| !bw.contains(w)).collect();
    if !extra.is_empty() {
        return extra.join(" ");
    }
    let (a, b) = (cls.to_lowercase(), base_cls.to_lowercase());
    let n = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    // whole words: 'rsi_pack_01_01_04' against '..._01_01_01' gives '04', not '4'
    let chars: Vec<char> = cls.chars().collect();
    let al: Vec<char> = a.chars().collect();
    let cut = al[..n.min(al.len())].iter().rposition(|c| *c == '_').map_or(0, |i| i + 1);
    let tail: String = chars[cut.min(chars.len())..].iter().collect();
    let tail = tail.trim_matches('_').to_string();
    let tail = if tail.is_empty() { cls.to_string() } else { tail };
    static R: OnceLock<Regex> = OnceLock::new();
    re(r"_+", &R).replace_all(&tail, " ").to_string()
}

/// `[{id, name, material, palette, cls, default}]` of an item: the default look first, then one entry per variant class whose look differs from
/// those before.  `id` is the lower-case class name ("" for the default).
pub fn skins(items: &ItemMaterials, stats: &ItemStats, weapons: &Weapons, p4k: &P4k, cls: &str, variants: &[&Value]) -> Vec<Value> {
    let Some(base) = look(items, stats, weapons, p4k, cls, "male", true) else { return vec![] };
    let mut out = vec![json!({"id": "", "name": "Default", "material": base["material"], "palette": base["palette"], "cls": cls, "default": true})];
    let mut seen: Vec<(Value, Value)> = vec![(base["material"].clone(), base["palette"].clone())];
    let bname = stats.identity(cls)["name"].as_str().unwrap_or("").to_string();
    let mut vs: Vec<&&Value> = variants.iter().collect();
    vs.sort_by(|a, b| s(g(a, "cls")).cmp(s(g(b, "cls"))));
    for v in vs {
        let vcls = s(g(v, "cls"));
        let Some(lk) = look(items, stats, weapons, p4k, vcls, "male", true) else { continue };
        let mat = if truthy(&lk["material"]) { lk["material"].clone() } else { base["material"].clone() };
        let pal = if truthy(&lk["palette"]) { lk["palette"].clone() } else { base["palette"].clone() };
        if seen.contains(&(mat.clone(), pal.clone())) {
            continue;
        }
        seen.push((mat.clone(), pal.clone()));
        out.push(json!({"id": vcls.to_lowercase(), "name": skin_name(&bname, s(g(v, "name")), vcls, cls), "material": mat, "palette": pal, "cls": vcls, "default": false}));
    }
    out
}

#[allow(dead_code)]
fn unused(_: Map<String, Value>) {}
