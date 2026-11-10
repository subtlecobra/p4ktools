//! Geometry path -> material paths, from the item / entity records of Game2.dcb.
//!
//! CONFIRMED against the engine and the SGeometryResourceParams component of EntityClassDefinition records: the material of a model
//! is not always the one named inside the .cgf; an entity record carries `Geometry.Material.path` next to `Geometry.Geometry.path`
//! (and per sub-geometry node).  A geometry usually appears in several records with several materials (paint
//! variants), so `candidates` returns all of them, commonest first; which one a placed entity uses cannot be known from the geometry
//! alone.
//!
//! EMPIRICAL: paths in the file are relative to Data/ and the material path carries '.mtl'.  A node with an empty material is read as
//! inheriting its parent's (INFERRED); only explicit pairs are indexed here.
//!
//! This loads eagerly in `load`: opening Game2.dcb and building the index takes well under a second.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use p4k_formats::dcb::{guid_str, Dcb, Obj, Record, Value, REC_SZ};
use p4k_formats::{Error, Result};
use p4k::P4k;
use serde_json::{json, Value as Json};

use crate::items::ItemMaterialSource;

pub const DCB_PATH: &str = "Data/Game2.dcb";
const PAL_ROOT: &str = "libs/foundry/records/tintpalettes/";

/// Lowercase, forward slashes, no leading '/' and no leading 'data/'.
pub fn key(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches('/').to_lowercase();
    p.strip_prefix("data/").map(str::to_owned).unwrap_or(p)
}

/// `{geometry key: [material path]}`, commonest first, ties by name.
pub fn build_index<I: IntoIterator<Item = (String, String)>>(pairs: I) -> HashMap<String, Vec<String>> {
    let mut counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for (g, m) in pairs {
        if !g.is_empty() && !m.is_empty() {
            let c = counts.entry(key(&g)).or_default();
            let mk = m.replace('\\', "/");
            let mut mk = mk.trim_start_matches('/').to_string();
            if mk.to_lowercase().ends_with(".mtl") {
                mk.truncate(mk.len() - 4);
            }
            *c.entry(mk).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .map(|(g, c)| {
            let mut v: Vec<(String, usize)> = c.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            (g, v.into_iter().map(|x| x.0).collect())
        })
        .collect()
}

pub struct ItemMaterials {
    dcb: Option<Dcb>,
    by_geo: HashMap<String, Vec<String>>,
    class_geo: Mutex<HashMap<String, Option<String>>>,
    guid_idx: OnceLock<HashMap<String, usize>>,
    pal_files: OnceLock<HashMap<String, usize>>,
}

impl ItemMaterials {
    /// Opens `Data/Game2.dcb` (an empty index when the archive has none) and indexes its geometry / material pairs.
    pub fn load(p4k: &P4k) -> Result<ItemMaterials> {
        let Some(ent) = p4k.get(DCB_PATH) else { return Ok(Self::from_pairs(vec![])) };
        let data = p4k.read(ent).map_err(|e| Error::msg(e.to_string()))?;
        let d = Dcb::open(data)?;
        let by_geo = build_index(d.geometry_pairs()?.into_iter().map(|(_, _, g, m)| (g, m)));
        Ok(ItemMaterials { dcb: Some(d), by_geo, class_geo: Mutex::new(HashMap::new()), guid_idx: OnceLock::new(), pal_files: OnceLock::new() })
    }

    /// Index from (geometry path, material path) pairs; for tests and for callers that already have them.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> ItemMaterials {
        ItemMaterials { dcb: None, by_geo: build_index(pairs), class_geo: Mutex::new(HashMap::new()), guid_idx: OnceLock::new(), pal_files: OnceLock::new() }
    }

    /// The shared DataCore reader, or None when the archive has no Game2.dcb (or the index was built from pairs).
    pub fn datacore(&self) -> Option<&Dcb> {
        self.dcb.as_ref()
    }

    /// Record index for a record GUID (the third element of a `Ref`; the number is an index inside a struct type, not a record index).
    /// A later record wins on a repeated GUID.
    pub fn guid_record(&self, guid: &str) -> Option<usize> {
        let d = self.dcb.as_ref()?;
        let idx = self.guid_idx.get_or_init(|| {
            let mut m = HashMap::with_capacity(d.n_rec);
            for i in 0..d.n_rec {
                let o = d.rec_off + i * REC_SZ + 16;
                m.insert(guid_str(&d.bytes()[o..o + 16]), i);
            }
            m
        });
        idx.get(guid).copied()
    }

    /// Entity class name of a default-loadout entry: its entityClassName, else the class its entityClassReference points to (EMPIRICAL:
    /// weapons and mounts are given by reference, with an empty name, e.g. Gladius hardpoint_gun_nose ->
    /// EntityClassDefinition.Mount_Gimbal_S3), else ''.
    pub fn entry_class(&self, entry: &Obj) -> String {
        if let Some(Value::Str(n)) = entry.get("entityClassName") {
            if !n.is_empty() {
                return n.clone();
            }
        }
        if let Some(Value::Ref(n, guid)) = entry.get("entityClassReference") {
            if *n != 0xFFFF_FFFF {
                if let (Some(i), Some(d)) = (self.guid_record(guid), self.dcb.as_ref()) {
                    if let Ok(name) = d.record_name(i) {
                        return name.strip_prefix("EntityClassDefinition.").map(str::to_owned).unwrap_or_default();
                    }
                }
            }
        }
        String::new()
    }

    /// `["Geometry"]["Geometry"]["path"]` of a SGeometryDataParams-shaped value: Err when a key is missing or a value has the wrong type.
    fn node_path(v: &Value) -> std::result::Result<String, ()> {
        match v.get("Geometry").and_then(|g| g.get("Geometry")).and_then(|g| g.get("path")) {
            Some(Value::Str(s)) => Ok(s.clone()),
            _ => Err(()),
        }
    }

    fn compute_class_geometry(&self, class_name: &str) -> Option<String> {
        let d = self.dcb.as_ref()?;
        let ids = d.record_index().get(&format!("EntityClassDefinition.{}", class_name))?;
        let rec: Record = d.record(*ids.first()?).ok()?;
        let data = d.record_data(&rec, 8).ok()?;
        let Some(Value::List(comps)) = data.get("Components") else { return None };
        for comp in comps {
            let Value::Obj(o) = comp else { continue };
            if o.struct_name != "SGeometryResourceParams" {
                continue;
            }
            let node = comp.get("Geometry")?;
            let mut path = Self::node_path(node).ok()?;
            if path.is_empty() {
                if let Some(Value::List(subs)) = node.get("SubGeometry") {
                    for s in subs {
                        if matches!(s, Value::Null) {
                            continue;
                        }
                        let p = Self::node_path(s).ok()?;
                        if !p.is_empty() {
                            path = p;
                            break;
                        }
                    }
                }
            }
            let low = path.to_lowercase();
            return if low.ends_with(".cgf") || low.ends_with(".cga") { Some(path) } else { None };
        }
        None
    }

    /// Geometry path (relative to Data/, as written in the record) of an entity class, or None.
    ///
    /// An entity in a .soc often names only its class (EntityClass='Seat_Bench_Backrest_3_seat_bench_1_a'); the model is in the record
    /// 'EntityClassDefinition.<class>': its SGeometryResourceParams component, Geometry node, Geometry path (the child
    /// nodes' sub-geometries are only read when the node itself names none).  Cached.
    pub fn class_geometry(&self, class_name: &str) -> Option<String> {
        if let Some(v) = self.class_geo.lock().unwrap().get(class_name) {
            return v.clone();
        }
        let out = self.compute_class_geometry(class_name);
        self.class_geo.lock().unwrap().insert(class_name.to_string(), out.clone());
        out
    }

    /// The palette key (`palette()` understands it) of TintPaletteTree record `i`.
    pub fn palette_key_of_record(&self, i: usize) -> Option<String> {
        self.palette_files().iter().find(|(_, &v)| v == i).map(|(k, _)| k.clone())
    }

    /// `{record file without '.xml', relative to the palettes folder, lowercase: record index}` for the 2,423 TintPaletteTree records.
    pub fn palette_files(&self) -> &HashMap<String, usize> {
        self.pal_files.get_or_init(|| {
            let mut pf = HashMap::new();
            if let Some(d) = self.dcb.as_ref() {
                let mut seen = std::collections::HashSet::new();
                for i in 0..d.n_rec {
                    let Ok(name) = d.record_name(i) else { continue };
                    if !name.starts_with("TintPaletteTree.") || !seen.insert(name.into_owned()) {
                        continue; // first record of each name only
                    }
                    let Ok(f) = d.record_file(i) else { continue };
                    let f = f.to_lowercase();
                    if let Some(rest) = f.strip_prefix(PAL_ROOT) {
                        if let Some(stem) = rest.strip_suffix(".xml") {
                            pf.insert(stem.to_string(), i);
                        }
                    }
                }
            }
            pf
        })
    }

    pub fn palette_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.palette_files().keys().cloned().collect();
        v.sort();
        v
    }

    /// Tint palette by the path a container lists ('Libs/Foundry/Records/TintPalettes/Generic/gen_matte_dark_a', any case, optional
    /// '<abc>' entry order) or by its name relative to the palettes folder; None when unknown.  Result: `{name, entries: [{t, s, g}]
    /// for entryA..C (0..1, sRGB as stored: SRGB8 bytes and gloss 0..255), glass: rgb, decal}`.  A layer with PaletteTint n
    /// (1..3) takes entry n-1; '<abc>' (digits 1..3) says which of the three entries feed the primary / secondary / tertiary tint.
    pub fn palette(&self, path: &str) -> Option<Json> {
        let low = path.replace('\\', "/").to_lowercase();
        let (base, sw) = low.split_once('<').unwrap_or((&low, ""));
        let base = base.trim();
        let (key, variant) = base.split_once('|').unwrap_or((base, ""));
        let order: Vec<usize> = sw.trim_end_matches(['>', ' ']).chars().take(3).filter(|c| "123".contains(*c)).map(|c| c as usize - '1' as usize).collect();
        let order = if order.len() == 3 { order } else { vec![0, 1, 2] };
        let key = key.strip_prefix(PAL_ROOT).unwrap_or(key);
        let key = key.strip_suffix(".xml").unwrap_or(key);
        let i = *self.palette_files().get(key)?;
        let d = self.dcb.as_ref()?;
        let data = d.record_data(&d.record(i).ok()?, 6).ok()?;
        let mut root: Option<&Value> = data.get("root").filter(|v| !matches!(v, Value::Null));
        if !variant.is_empty() {
            let Some(Value::List(children)) = root.and_then(|r| r.get("children")) else { return None };
            root = children.iter().find(|c| !matches!(c, Value::Null) && c.get("name").and_then(Value::as_str).map_or(String::new(), str::to_lowercase) == variant);
            root?;
        }
        let root = root;
        let rgb = |c: Option<&Value>| -> Json {
            let g = |k: &str| c.and_then(|c| c.get(k)).and_then(Value::as_f64).unwrap_or(0.0);
            json!([round4(g("r") / 255.0), round4(g("g") / 255.0), round4(g("b") / 255.0)])
        };
        let mut entries = vec![];
        for k in ["entryA", "entryB", "entryC"] {
            let e = root?.get(k)?;
            let t = e.get("tintColor")?;
            let s = e.get("specColor")?;
            let g = e.get("glossiness")?.as_f64()?;
            entries.push(json!({"t": rgb(Some(t)), "s": rgb(Some(s)), "g": round4(g / 255.0)}));
        }
        let entries: Vec<Json> = order.iter().map(|&k| entries[k].clone()).collect();
        fn nn(v: Option<&Value>) -> Option<&Value> {
            v.filter(|v| !matches!(v, Value::Null))
        }
        let root = root?;
        let dtex = root.get("decalTexture").and_then(Value::as_str).unwrap_or("").replace('\\', "/");
        let decal = if dtex.is_empty() {
            Json::Null
        } else {
            json!({"tex": dtex, "col": [rgb(nn(root.get("decalColorR"))), rgb(nn(root.get("decalColorG"))), rgb(nn(root.get("decalColorB")))]})
        };
        Some(json!({"name": format!("{}{}", key, if variant.is_empty() { String::new() } else { format!("|{}", variant) }),
            "entries": entries, "glass": rgb(nn(root.get("glassColor"))), "decal": decal}))
    }

    /// Material paths (no extension, relative to Data/ as written in the records) for a geometry, commonest first.
    pub fn candidates(&self, geometry_name: &str) -> &[String] {
        self.by_geo.get(&key(geometry_name)).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Number of distinct geometry keys in the index.
    pub fn geometry_count(&self) -> usize {
        self.by_geo.len()
    }
}

/// Rounding like Python's `round(x, 4)` (correctly rounded decimal of the binary value, ties to even).
fn round4(x: f64) -> f64 {
    format!("{:.4}", x).parse().unwrap_or(x)
}

impl ItemMaterialSource for ItemMaterials {
    fn candidates(&self, geometry_name: &str) -> Vec<String> {
        ItemMaterials::candidates(self, geometry_name).to_vec()
    }
    fn class_geometry(&self, class_name: &str) -> Option<String> {
        ItemMaterials::class_geometry(self, class_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_commonest_first() {
        let im = ItemMaterials::from_pairs(vec![
            ("Data\\Objects\\A.cgf".into(), "objects/m1.mtl".into()),
            ("objects/a.cgf".into(), "objects/m2.mtl".into()),
            ("objects/a.cgf".into(), "Objects/M2.mtl".into()),
            ("objects/a.cgf".into(), "objects/m2.mtl".into()),
            ("objects/b.cgf".into(), "".into()),
        ]);
        assert_eq!(im.candidates("data/objects/a.cgf"), ["objects/m2", "Objects/M2", "objects/m1"]);
        assert!(im.candidates("objects/b.cgf").is_empty());
        assert!(im.class_geometry("x").is_none());
    }

    #[test]
    fn round_matches_python() {
        assert_eq!(round4(0.5), 0.5);
        assert_eq!(round4(100.0 / 255.0), 0.3922);
    }
}
