//! Ship liveries (paints): which tint palette and material a ship wears, as the zone compile needs
//! it (`resolve`, `class_palette`, `liveries` without the localised names).
//!
//! Layout (EMPIRICAL on the live archive, 1,147 ship records and 1,132 `Paint_*` item records): a ship's `SGeometryResourceParams` has a
//! main geometry node and `SubGeometry` nodes, each with a material and a tint palette; a `Paint_*` item's class name and tags identify the
//! sub-node it selects; the default loadout names the paint in `hardpoint_paint`.  A sub-node's missing value falls back to the main node's
//! (INFERRED).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use p4k_formats::dcb::{Value as Dv, NULL_PTR};

use crate::itemmaterials::ItemMaterials;

const PAINT_PREFIX: &str = "EntityClassDefinition.Paint_";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaintNode {
    pub tags: Vec<String>,
    pub material: Option<String>,
    pub palette: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ShipNodes {
    pub main: PaintNode,
    pub subs: Vec<PaintNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Livery {
    pub id: String,
    pub tag: String,
    pub material: Option<String>,
    pub palette: Option<String>,
    pub default: bool,
    /// the display name: the localised one when a `localise` callback is given and does not return a placeholder, else the fallback
    pub name: String,
    /// set by `annotate`: comparable material path ("" = the hull's own material or none)
    pub mkey: Option<String>,
}

impl Livery {
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({"id": self.id, "name": self.name, "tag": self.tag, "material": self.material, "palette": self.palette, "default": self.default});
        if let Some(k) = &self.mkey {
            v["mkey"] = serde_json::json!(k);
        }
        v
    }
}

/// `Paint_*` items: `{lower-case key: paint class}` and `{class: (localisation key of the name, RequiredTags)}`.
#[derive(Debug, Default)]
pub struct PaintItems {
    pub keys: HashMap<String, String>,
    pub info: HashMap<String, (Option<String>, String)>,
}

fn ref_class(items: &ItemMaterials, r: Option<&Dv>) -> Option<String> {
    if let Some(Dv::Ref(n, g)) = r {
        if *n != NULL_PTR {
            let i = items.guid_record(g)?;
            return items.datacore()?.record_name(i).ok().map(|n| n.into_owned());
        }
    }
    None
}

/// Key `ItemMaterials::palette` understands for a `TintPaletteRef`, or None when unset or not a TintPaletteTree.
pub fn palette_key(items: &ItemMaterials, palette: Option<&Dv>) -> Option<String> {
    let p = match palette {
        Some(o @ Dv::Obj(_)) => o,
        _ => return None,
    };
    let root = p.get("RootRecord");
    let name = ref_class(items, root)?;
    if !name.starts_with("TintPaletteTree.") {
        return None;
    }
    let Some(Dv::Ref(_, g)) = root else { return None };
    let i = items.guid_record(g)?;
    let mut key = items.palette_key_of_record(i)?;
    if let Some(cp) = p.get("ChildPath").and_then(Dv::as_str).filter(|c| !c.is_empty()) {
        key = format!("{}|{}", key, cp.to_lowercase());
    }
    Some(key)
}

fn node_of(items: &ItemMaterials, n: &Dv) -> PaintNode {
    let g = n.get("Geometry");
    let mtl = g.and_then(|g| g.get("Material")).and_then(|m| m.get("path")).and_then(Dv::as_str).unwrap_or("").replace('\\', "/");
    PaintNode {
        tags: n.get("Tags").and_then(Dv::as_str).unwrap_or("").split_whitespace().map(str::to_owned).collect(),
        material: if mtl.is_empty() { None } else { Some(mtl) },
        palette: palette_key(items, g.and_then(|g| g.get("Palette"))),
    }
}

/// Main node and sub-nodes of the `SGeometryResourceParams` of an entity class (the ship, or any item), None when it has none.
pub fn ship_nodes(items: &ItemMaterials, cls: &str) -> Option<ShipNodes> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<ShipNodes>>>> = OnceLock::new();
    let _ = &CACHE;
    let d = items.datacore()?;
    if cls.is_empty() {
        return None;
    }
    let ids = d.record_index().get(&format!("EntityClassDefinition.{}", cls))?;
    let data = d.record_data(&d.record(ids[0]).ok()?, 12).ok()?;
    let Some(Dv::List(comps)) = data.get("Components") else { return None };
    for c in comps {
        if matches!(c, Dv::Obj(o) if o.struct_name == "SGeometryResourceParams") {
            let top = c.get("Geometry").filter(|t| !matches!(t, Dv::Null))?;
            let subs = match top.get("SubGeometry") {
                Some(Dv::List(l)) => l.iter().map(|s| node_of(items, s)).collect(),
                _ => vec![],
            };
            return Some(ShipNodes { main: node_of(items, top), subs });
        }
    }
    None
}

/// Every `Paint_*` item: `{lower-case key: paint class}` (the class name and each of its tags without a leading '@'; first wins) and
/// `{class: (name key, requires)}`.
pub fn paint_items(items: &ItemMaterials) -> PaintItems {
    let mut out = PaintItems::default();
    let Some(d) = items.datacore() else { return out };
    let mut recs: Vec<(&String, &Vec<usize>)> = d.record_index().iter().filter(|(k, _)| k.starts_with(PAINT_PREFIX)).collect();
    recs.sort_by_key(|(_, ids)| ids[0]);
    for (rec, ids) in recs {
        let cls = &rec["EntityClassDefinition.".len()..];
        let Some(data) = d.record(ids[0]).ok().and_then(|r| d.record_data(&r, 4).ok()) else { continue };
        let Some(Dv::List(comps)) = data.get("Components") else { continue };
        let att = comps.iter().find(|c| matches!(c, Dv::Obj(o) if o.struct_name == "SAttachableComponentParams"));
        let a = att.and_then(|a| a.get("AttachDef"));
        if a.and_then(|a| a.get("Type")).and_then(Dv::as_str) != Some("Paints") {
            continue;
        }
        let tags = a.and_then(|a| a.get("Tags")).and_then(Dv::as_str).unwrap_or("");
        for k in std::iter::once(cls).chain(tags.split_whitespace()) {
            out.keys.entry(k.trim_start_matches('@').to_lowercase()).or_insert_with(|| cls.to_string());
        }
        let name = a.and_then(|a| a.get("Localization")).and_then(|l| l.get("Name")).and_then(Dv::as_str).map(str::to_string);
        out.info.insert(cls.to_string(), (name, a.and_then(|a| a.get("RequiredTags")).and_then(Dv::as_str).unwrap_or("").to_string()));
    }
    out
}

/// 'Paint_Mole_Yellow_Grey_Black' -> 'Yellow Grey Black' (the fallback name).
pub fn pretty(cls: &str) -> String {
    let parts: Vec<&str> = cls.split('_').skip(1).collect();
    if parts.len() > 1 { parts[1..].join(" ") } else { parts.join(" ") }
}

/// The paint item named by the `hardpoint_paint` entry of a default loadout, or ''.
pub fn default_paint_class(items: &ItemMaterials, entries: &[Dv]) -> String {
    for e in entries {
        if let Dv::Obj(o) = e {
            if o.get("itemPortName").and_then(Dv::as_str).unwrap_or("").to_lowercase() == "hardpoint_paint" {
                return items.entry_class(o);
            }
        }
    }
    String::new()
}

/// Liveries of a ship class, the default first.
pub fn liveries(items: &ItemMaterials, cls: &str, loadout_entries: Option<&[Dv]>) -> Vec<Livery> {
    liveries_with(items, cls, loadout_entries, None)
}

/// `liveries` with the localisation callback (`@key` -> English text) the names are taken from; placeholders fall back to `pretty`.
pub fn liveries_with(items: &ItemMaterials, cls: &str, loadout_entries: Option<&[Dv]>, localise: Option<&dyn Fn(&str) -> Option<String>>) -> Vec<Livery> {
    let Some(nodes) = ship_nodes(items, cls) else { return vec![] };
    let table = paint_items(items);
    let keys = &table.keys;
    let main = &nodes.main;
    let mut out: Vec<Livery> = vec![];
    for s in &nodes.subs {
        let Some(paint) = s.tags.iter().find_map(|t| keys.get(&t.to_lowercase())) else { continue };
        out.push(Livery {
            id: paint.clone(),
            name: {
                let key = table.info.get(paint).and_then(|i| i.0.clone()).unwrap_or_default();
                let nm = localise.and_then(|f| f(&key)).unwrap_or_default();
                if nm.is_empty() || nm.contains("PLACEHOLDER") {
                    let p = pretty(paint);
                    if p.is_empty() { paint.clone() } else { p }
                } else {
                    nm
                }
            },
            tag: s.tags.join(" "),
            material: s.material.clone().or_else(|| main.material.clone()),
            palette: s.palette.clone().or_else(|| main.palette.clone()),
            default: false,
            mkey: None,
        });
    }
    let want = loadout_entries.map(|e| default_paint_class(items, e)).unwrap_or_default();
    let mut pick: Option<usize> = if want.is_empty() { None } else { out.iter().position(|o| o.id == want) };
    if pick.is_none() {
        pick = out.iter().position(|o| o.material == main.material && o.palette == main.palette);
    }
    let idx = match pick {
        Some(i) => i,
        None => {
            out.insert(0, Livery { id: String::new(), name: "Standard".into(), tag: String::new(), material: main.material.clone(), palette: main.palette.clone(), default: true, mkey: None });
            0
        }
    };
    out[idx].default = true;
    // sorted by (not default, lower-cased name); the sort is stable
    out.sort_by(|a, b| (!a.default, a.name.to_lowercase()).cmp(&(!b.default, b.name.to_lowercase())));
    out
}

/// The livery a ship is compiled with: the paint class `paint` ('' / None = the default one), or None when not one of the ship's liveries.
pub fn resolve(items: &ItemMaterials, cls: &str, paint: Option<&str>, loadout_entries: Option<&[Dv]>) -> Option<Livery> {
    let lv = liveries(items, cls, loadout_entries);
    match paint.filter(|p| !p.is_empty()) {
        None => lv.into_iter().find(|o| o.default),
        Some(p) => lv.into_iter().find(|o| o.id == p),
    }
}

/// Palette key of an item's own main geometry node (a weapon with its own livery), or None.
pub fn class_palette(items: &ItemMaterials, cls: &str) -> Option<String> {
    ship_nodes(items, cls).and_then(|n| n.main.palette)
}

/// Material path in a comparable form (lower case, no leading slash / 'data/', no extension); "" for none.
pub fn material_key(path: &str) -> String {
    let p = path.replace('\\', "/").to_lowercase();
    let p = p.trim_start_matches('/');
    let p = p.strip_prefix("data/").unwrap_or(p);
    p.strip_suffix(".mtl").unwrap_or(p).to_string()
}

/// Adds `mkey` to each livery: the comparable material path it needs, "" when it is the hull model's own material (`own_material`, a
/// material_key) or none.  Two liveries with the same mkey differ only by their palette (a palette-slot swap in the zone viewer).
pub fn annotate(lv: &mut [Livery], own_material: &str) {
    for o in lv.iter_mut() {
        let k = material_key(o.material.as_deref().unwrap_or(""));
        o.mkey = Some(if k == own_material { String::new() } else { k });
    }
}

/// material_key of the material the hull model (the ship record's .cga) names in its own material chunk, resolved like the zone compiler does;
/// "" when unreadable.
pub fn hull_material_key(items: &ItemMaterials, p4k: &p4k::P4k, hull: &str) -> String {
    let ent = p4k.find_path(hull, ".cga").or_else(|| p4k.find_path(hull, ".cgf"));
    let Some(ent) = ent else { return String::new() };
    let Ok(data) = p4k.read(ent) else { return String::new() };
    let Ok(Some(mp)) = crate::builder::material_path(&data) else { return String::new() };
    let (res, _) = crate::builder::resolve_material_path(p4k, &ent.name, Some(&mp), Some(items as &dyn crate::items::ItemMaterialSource));
    res.map(|r| material_key(&r)).unwrap_or_default()
}
