//! The 3D model of a weapon with the attachments on its ports, and the colours of its materials for a skin.
//! Evidence notes (EMPIRICAL / INFERRED):
//!
//! * A first-person weapon is a `.cdf` (skeleton + skinned parts) whose bind pose is the weapon at rest; its helper bones (`magAttach`,
//!   `sight_attachment` ...) are the frames the attachments hang on (the port definition names the bone, `Ships::port_helpers`).  A ship
//!   weapon is a `.cgf` / `.cga` whose nodes are the frames.
//! * INFERRED (as for ship loadouts): an attachment's model is authored with its origin on the port's frame, so it is drawn with the frame's
//!   matrix (and the port's `Offset`).  An attachment is itself a model with ports: its own default loadout fills them.
//! * A piece attached to a bone named `bullet*` (the chambered round) is left out (INFERRED).
//! * Materials: one *slot* per (material file, sub-material); `slot_materials` turns the slots into materials, with the layered description
//!   and texture list the item pages evaluate when `lazy` (see `slot_materials`).

use std::collections::{BTreeSet, HashMap};

use p4k_formats::{cdf, ivo, skeleton, socpak};
use p4k::P4k;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::builder::{self, Part};
use crate::encode;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::ItemStats;
use crate::materials::{rnd, MatRegistry};
use crate::ships::Ships;
use crate::swaps::{self, Port};
use crate::weapons::Weapons;

pub type Mat = [f64; 12];
pub const IDENT: Mat = socpak::IDENT;
pub const MAX_DEPTH: usize = 3;
const SKIP_BONES: [&str; 1] = ["bullet"];
const DECAL_SHADERS: [&str; 3] = ["MeshDecal", "Decal", "WaterDecal"];
pub const DEFAULT_PALETTE: &str = "default";

/// 3x4 row-major matrix (12 floats) of the unit quaternion q (x, y, z, w) and the translation p.
pub fn row_major(q: [f64; 4], p: [f64; 3]) -> Mat {
    let m = skeleton::q_to_mat(skeleton::qnorm(q), p); // column-major 4x4: m[4 * column + row]
    [m[0], m[4], m[8], p[0], m[1], m[5], m[9], p[1], m[2], m[6], m[10], p[2]]
}

/// 3x4 row-major matrix of a `.cdf` attachment's RelPosition / RelRotation (w, x, y, z).
pub fn cdf_matrix(pos: [f64; 3], rot_wxyz: [f64; 4]) -> Mat {
    let [w, x, y, z] = rot_wxyz;
    row_major([x, y, z, w], pos)
}

/// One part of an item model: vertices, triangles, uv, the material key (material file, sub-material) and the stored tangent frame.
#[derive(Debug, Clone, Default)]
pub struct MPart {
    pub name: String,
    pub verts: Vec<[f64; 3]>,
    pub tris: Vec<[u32; 3]>,
    pub uv: Vec<[f64; 2]>,
    pub key: Option<(String, i64)>,
    pub nrm: Vec<[f64; 3]>,
    pub tan: Vec<[f64; 4]>,
    pub vc: Vec<[u8; 4]>,
    /// `skin_parts` only: the index in the .skinm vertex stream each output vertex was copied from (skin weights are looked up there; `heldskin.rs`).
    pub src: Vec<u32>,
}

/// One item's model in its own space: `parts` and `frames` {lower-case helper name: 3x4 matrix}, plus `notes`.
#[derive(Debug, Clone, Default)]
pub struct ItemModel {
    pub parts: Vec<MPart>,
    pub frames: HashMap<String, Mat>,
    pub notes: Vec<String>,
}

/// Copy of an item-model part with its vertices and its stored tangent frame (when both `nrm` and `tan` are present) moved by `m`.
pub fn move_part(pt: &MPart, m: &Mat) -> MPart {
    let mut out = pt.clone();
    out.verts = pt.verts.iter().map(|v| ivo::xform(m, *v)).collect();
    if !pt.nrm.is_empty() && !pt.tan.is_empty() {
        let mut nrm = vec![];
        let mut tan = vec![];
        for (n, t) in pt.nrm.iter().zip(pt.tan.iter()) {
            let f = ivo::rotate_frame(m, ivo::Frame { n: *n, t: [t[0], t[1], t[2]], h: t[3] });
            nrm.push(f.n);
            tan.push([f.t[0], f.t[1], f.t[2], f.h]);
        }
        out.nrm = nrm;
        out.tan = tan;
    }
    out
}

fn strip_mtl(p: &str) -> String {
    if p.to_lowercase().ends_with(".mtl") {
        p[..p.len() - 4].to_string()
    } else {
        p.to_string()
    }
}

/// ItemModel of a .cgf / .cga (render parts only; collision and NoDraw proxies are skipped).
pub fn cgf_model(p4k: &P4k, items: &ItemMaterials, path: &str, material: Option<&str>) -> Result<ItemModel, String> {
    let mut out = ItemModel::default();
    let ext = path.rfind('.').map(|i| path[i..].to_lowercase()).unwrap_or_default();
    let Some(ent) = p4k.find_path(path, &ext) else {
        out.notes.push(format!("model not in the archive: {}", path));
        return Ok(out);
    };
    let mut reg = MatRegistry::with_options(p4k, 256, false, false);
    reg.items = Some(items);
    let opts = builder::Options { with_render: true, with_collision: false, mtl_override: material.map(str::to_string), ..Default::default() };
    let (info, parts) = builder::build_scene(p4k, ent, &opts, &mut reg)?;
    let keys = reg.mat_keys();
    for pt in parts {
        if pt.kind != "render" || pt.tris.is_empty() {
            continue;
        }
        out.parts.push(MPart {
            name: pt.name.clone(),
            verts: pt.verts.clone(),
            tris: pt.tris.clone(),
            uv: pt.uv.clone(),
            key: keys.get(&pt.mat.unwrap_or(-1)).cloned(),
            nrm: pt.nrm.clone(),
            tan: pt.tan.clone(),
            vc: pt.vc.clone(),
            src: vec![],
        });
    }
    out.notes.extend(info.notes.iter().cloned());
    match p4k.read(ent).map_err(|e| e.to_string()).and_then(|b| ivo::parse_cgf(&b, false).map_err(|e| e.to_string())) {
        Ok(cg) => {
            for n in &cg.nodes {
                out.frames.insert(n.name.to_lowercase(), n.world);
            }
        }
        Err(ex) => out.notes.push(format!("node table unreadable: {}", ex)),
    }
    Ok(out)
}

/// Material path (no extension) stored in the .skin's own material chunk, resolved like a cgf's, or "".
///
/// EMPIRICAL: a .cdf whose CA_SKIN attachment has an empty Material and whose cdf has none either (Volt Parallax
/// rifle) leaves the skin's own material to apply, exactly as a rigid attachment keeps the material of its .cgf.
pub fn skin_own_material(p4k: &P4k, items: &ItemMaterials, skin_path: &str) -> String {
    let Some(ent) = p4k.find_path(skin_path, ".skin") else { return String::new() };
    let Ok(data) = p4k.read(ent) else { return String::new() };
    let Ok(Some(name)) = builder::material_path(&data) else { return String::new() };
    if name.is_empty() {
        return String::new();
    }
    builder::resolve_material_path(p4k, &skin_path.replace('\\', "/"), Some(&name), Some(items as &dyn crate::items::ItemMaterialSource)).0.unwrap_or_default()
}

/// Render parts of a .skin / .skinm pair in the bind pose, grouped by sub-material.
pub fn skin_parts(p4k: &P4k, skin_path: &str, mtl_path: &str) -> Vec<MPart> {
    let stem = &skin_path[..skin_path.rfind('.').unwrap_or(skin_path.len())];
    let Some(ent) = p4k.find_path(&format!("{}.skinm", stem), ".skinm") else { return vec![] };
    let Ok(data) = p4k.read(ent) else { return vec![] };
    let Ok(meshes) = ivo::parse_cgfm(&data) else { return vec![] };
    let Some(m) = meshes.first() else { return vec![] };
    let (Ok(pos), Ok(uvs), Ok(idx)) = (m.positions(), m.uvs(), m.indices()) else { return vec![] };
    let cols = m.colors().unwrap_or_default();
    let frames = ivo::FrameReader::new(m);
    let mut groups: std::collections::BTreeMap<u32, Vec<&ivo::Subset>> = std::collections::BTreeMap::new();
    for s in &m.subsets {
        groups.entry(s.mat_id & 0xFFFF).or_default().push(s);
    }
    let name = stem.rsplit('/').next().unwrap_or(stem).to_string();
    let mut parts = vec![];
    for (sub, subs) in groups {
        let mut vmap: HashMap<i64, u32> = HashMap::new();
        let mut p = MPart { name: name.clone(), key: Some((mtl_path.to_string(), sub as i64)), ..Default::default() };
        for s in subs {
            let fi = s.first_index as usize;
            let seg = &idx[fi.min(idx.len())..(fi + s.num_indices as usize).min(idx.len())];
            let base = ivo::subset_index_base(s, seg);
            for t in 0..(s.num_indices as usize / 3) {
                let mut tri = [0u32; 3];
                for k in 0..3 {
                    let Some(&ix) = idx.get(fi + 3 * t + k) else { continue };
                    let vi = base + ix as i64;
                    let id = *vmap.entry(vi).or_insert_with(|| {
                        let n = p.verts.len() as u32;
                        let u = vi as usize;
                        p.verts.push(pos.get(u).copied().unwrap_or([0.0; 3]));
                        p.src.push(u as u32);
                        p.uv.push(uvs.get(u).copied().unwrap_or([0.0, 0.0]));
                        if !cols.is_empty() {
                            p.vc.push(cols.get(u).map(|c| ivo::bgra_to_rgba(*c)).unwrap_or([0xFF; 4]));
                        }
                        if let Some(fr) = &frames {
                            if let Ok(f) = fr.get(u) {
                                p.nrm.push(f.n);
                                p.tan.push([f.t[0], f.t[1], f.t[2], f.h]);
                            }
                        }
                        n
                    });
                    tri[k] = id;
                }
                p.tris.push(tri);
            }
        }
        parts.push(p);
    }
    parts
}

/// ItemModel of a .cdf: the skinned parts in the bind pose, the rigid pieces on their bones, the bones as frames.
pub fn cdf_model(p4k: &P4k, items: &ItemMaterials, path: &str, material: Option<&str>) -> Result<ItemModel, String> {
    let mut out = ItemModel::default();
    let Some(ent) = p4k.find_path(path, ".cdf") else {
        out.notes.push(format!("model not in the archive: {}", path));
        return Ok(out);
    };
    let c = cdf::parse_cdf(&p4k.read(ent).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let default_mtl = strip_mtl(&material.filter(|m| !m.is_empty()).unwrap_or(&c.material).replace('\\', "/"));
    let low = c.model.to_lowercase();
    let rigid = low.ends_with(".cga") || low.ends_with(".cgf");
    if rigid {
        // EMPIRICAL: <Model File> of 6 listed weapons is a rigid .cga / .cgf, not a skeleton; the model is the body
        // and its nodes are the frames, exactly as for a ship gun.  Their cdf Material is empty: the model's own material applies.
        let mat = material.filter(|m| !m.is_empty()).or(Some(c.material.as_str()).filter(|m| !m.is_empty()));
        let base = cgf_model(p4k, items, &c.model, mat)?;
        out.parts.extend(base.parts);
        out.frames.extend(base.frames);
        out.notes.extend(base.notes);
    }
    let chr_ent = if !c.model.is_empty() && !rigid { p4k.find_path(&c.model, ".chr") } else { None };
    let mut bones = vec![];
    if let Some(e) = chr_ent {
        match p4k.read(e).map_err(|e| e.to_string()).and_then(|b| skeleton::parse_bones(&b).map_err(|e| e.to_string())) {
            Ok(b) => bones = b,
            Err(ex) => out.notes.push(format!("skeleton unreadable: {}", ex)),
        }
    }
    for b in &bones {
        out.frames.insert(b.name.to_lowercase(), row_major(b.aq, b.ap));
    }
    for a in &c.attachments {
        if a.atype == "CA_SKIN" && !a.binding.is_empty() {
            let pick = material.filter(|m| !m.is_empty()).or(Some(a.material.as_str()).filter(|m| !m.is_empty())).unwrap_or(&default_mtl);
            let mut mtl_path = strip_mtl(&pick.replace('\\', "/"));
            if mtl_path.is_empty() {
                mtl_path = skin_own_material(p4k, items, &a.binding);
            }
            out.parts.extend(skin_parts(p4k, &a.binding, &mtl_path));
        } else if a.atype == "CA_BONE" && !a.binding.is_empty() && !SKIP_BONES.iter().any(|s| a.bone.to_lowercase().starts_with(s)) {
            let Some(frame) = out.frames.get(&a.bone.to_lowercase()).copied() else {
                out.notes.push(format!("bone {} of attachment {} not in the skeleton", a.bone, a.name));
                continue;
            };
            let sub = cgf_model(p4k, items, &a.binding, Some(a.material.as_str()).filter(|m| !m.is_empty()))?;
            let m = socpak::compose(&frame, &cdf_matrix(a.pos, a.rot));
            out.parts.extend(sub.parts.iter().map(|pt| move_part(pt, &m)));
        }
    }
    if out.parts.is_empty() {
        out.notes.push(format!("no renderable part in {}", path));
    }
    Ok(out)
}

/// ItemModel of the model file `path` (`.cdf` / `.cgf` / `.cga`).
pub fn load_model(p4k: &P4k, items: &ItemMaterials, path: &str, material: Option<&str>) -> Result<ItemModel, String> {
    if path.to_lowercase().ends_with(".cdf") {
        cdf_model(p4k, items, path, material)
    } else {
        cgf_model(p4k, items, path, material)
    }
}

// ---------------------------------------------------------------------------------------------------------------- loadout
/// One row of `port_state`.
#[derive(Debug, Clone)]
pub struct PortRow {
    pub path: String,
    pub port: String,
    pub cls: String,
    pub default: String,
    pub m: Option<Mat>,
    pub depth: usize,
    pub parent: String,
    pub helper: String,
    pub frame: bool,
    pub port_def: Port,
}

/// Everything the weapon / gear / outfit code reads: the shared DataCore, the archive and the per-DataCore caches.
pub struct Ctx<'a> {
    pub items: &'a ItemMaterials,
    pub stats: &'a ItemStats<'a>,
    pub p4k: &'a P4k,
    pub weapons: &'a Weapons<'a>,
    pub ships: &'a Ships<'a>,
}

/// Flat list of the ports of `cls` and everything on them, in port order.
///
/// `load` = {port path: class or ''} overrides (path = lower-case port names from the root joined by '/'); '' = empty.  The default item of a
/// port is the root's default-loadout entry for it; the item on a port brings its own default loadout (down to MAX_DEPTH levels).  With
/// `models` None no model is read (frames are unknown, `m` stays None).
pub fn port_state(ctx: &Ctx, cls: &str, load: &HashMap<String, String>, depth: usize, parent_path: &str, parent_m: Mat,
                  models: &mut Option<&mut HashMap<String, Option<ItemModel>>>, out: &mut Vec<PortRow>) {
    let table = swaps::item_ports(ctx.stats, cls);
    let helpers = ctx.ships.port_helpers(cls);
    let mut defaults: HashMap<String, String> = HashMap::new();
    for e in ctx.weapons.default_entries(cls) {
        let n = e.get("itemPortName").and_then(Value::as_str).unwrap_or("").to_lowercase();
        defaults.insert(n, ctx.weapons.entry_class(&e));
    }
    let parent_frames: Option<HashMap<String, Mat>> = models.as_ref().and_then(|m| m.get(cls)).and_then(|m| m.as_ref()).map(|m| m.frames.clone());
    for (name, port) in table {
        if port.types.is_empty() && !defaults.contains_key(&name) {
            continue;
        }
        let path = if parent_path.is_empty() { name.clone() } else { format!("{}/{}", parent_path, name) };
        let dflt = defaults.get(&name).cloned().unwrap_or_default();
        let cur = load.get(&path).cloned().unwrap_or_else(|| dflt.clone());
        let (helper, off) = helpers.get(&name).cloned().unwrap_or((name.clone(), None));
        let frame = parent_frames.as_ref().and_then(|f| f.get(&helper.to_lowercase()).or_else(|| f.get(&name)).copied());
        let mut m = None;
        if !cur.is_empty() {
            if let Some(f) = frame {
                let mm = socpak::compose(&parent_m, &f);
                m = Some(match off {
                    Some(o) => socpak::compose(&mm, &o),
                    None => mm,
                });
            }
        }
        out.push(PortRow { path: path.clone(), port: name.clone(), cls: cur.clone(), default: dflt, m, depth, parent: parent_path.to_string(), helper, frame: frame.is_some(), port_def: port });
        if !cur.is_empty() && depth + 1 < MAX_DEPTH {
            if let Some(ms) = models.as_deref_mut() {
                if !ms.contains_key(&cur) {
                    let mp = ctx.weapons.model_path(ctx.p4k, &cur);
                    let model = mp.and_then(|p| load_model(ctx.p4k, ctx.items, &p, None).ok());
                    ms.insert(cur.clone(), model);
                }
            }
            let go = match models.as_ref() {
                Some(ms) => ms.get(&cur).map_or(false, |m| m.is_some()),
                None => true,
            };
            if go {
                port_state(ctx, &cur, load, depth + 1, &path, m.unwrap_or(parent_m), models, out);
            }
        }
    }
}

/// `port_state` without models (the placement is not computed): what the clean-up of a `load=` string needs.
pub fn port_rows(ctx: &Ctx, cls: &str, load: &HashMap<String, String>) -> Vec<PortRow> {
    let mut out = vec![];
    port_state(ctx, cls, load, 0, "", IDENT, &mut None, &mut out);
    out
}

// ---------------------------------------------------------------------------------------------------------------- scene
#[derive(Debug, Clone)]
pub struct ScenePart {
    pub name: String,
    pub verts: Vec<[f64; 3]>,
    pub tris: Vec<[u32; 3]>,
    pub uv: Vec<[f64; 2]>,
    pub slot: usize,
    pub nrm: Vec<[f64; 3]>,
    pub tan: Vec<[f64; 4]>,
    pub vc: Vec<[u8; 4]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub src: String,
    pub cls: Option<String>,
    pub mtl: Option<String>,
    pub sub: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub parts: Vec<ScenePart>,
    pub slots: Vec<Slot>,
    pub rows: Vec<PortRow>,
    pub notes: Vec<String>,
    pub reason: String,
}

/// The weapon `cls` with the overrides `load`.  `src` of a slot is "body" for the weapon itself or the port path of the attachment that uses
/// it (a skin applies to the body only).
pub fn build_scene(ctx: &Ctx, cls: &str, load: &HashMap<String, String>) -> Result<Scene, String> {
    let mut models: HashMap<String, Option<ItemModel>> = HashMap::new();
    let root_path = ctx.weapons.model_path(ctx.p4k, cls);
    let root = match &root_path {
        Some(p) => load_model(ctx.p4k, ctx.items, p, None)?,
        None => ItemModel::default(),
    };
    models.insert(cls.to_string(), Some(root.clone()));
    let mut rows = vec![];
    {
        let mut mm = Some(&mut models);
        port_state(ctx, cls, load, 0, "", IDENT, &mut mm, &mut rows);
    }
    let mut sc = Scene::default();
    let mut slot_idx: HashMap<(String, Option<(String, i64)>), usize> = HashMap::new();
    let mut notes: Vec<String> = root.notes.clone();
    let mut src_cls: HashMap<String, String> = HashMap::new();
    src_cls.insert("body".into(), cls.to_string());
    for r in &rows {
        if !r.cls.is_empty() {
            src_cls.insert(r.path.clone(), r.cls.clone());
        }
    }
    let mut add = |sc: &mut Scene, model: &ItemModel, matrix: Option<&Mat>, src: &str| {
        for pt in &model.parts {
            let sk = (src.to_string(), pt.key.clone());
            let slot = *slot_idx.entry(sk).or_insert_with(|| {
                sc.slots.push(Slot { src: src.to_string(), cls: src_cls.get(src).cloned(), mtl: pt.key.as_ref().map(|k| k.0.clone()), sub: pt.key.as_ref().map_or(-1, |k| k.1) });
                sc.slots.len() - 1
            });
            let mv = match matrix {
                None => pt.clone(),
                Some(m) => move_part(pt, m),
            };
            sc.parts.push(ScenePart { name: pt.name.clone(), verts: mv.verts, tris: pt.tris.clone(), uv: pt.uv.clone(), slot, nrm: mv.nrm, tan: mv.tan, vc: pt.vc.clone() });
        }
    };
    add(&mut sc, &root, None, "body");
    for r in &rows {
        let model = models.get(&r.cls).and_then(|m| m.as_ref());
        if !r.cls.is_empty() && r.m.is_some() && model.is_some() {
            let model = model.unwrap();
            add(&mut sc, model, r.m.as_ref(), &r.path);
            notes.extend(model.notes.iter().cloned());
        } else if !r.cls.is_empty() && r.m.is_none() {
            notes.push(format!("port {}: no frame '{}' on the parent's model, {} not drawn", r.path, r.helper, r.cls));
        }
    }
    let set: BTreeSet<String> = notes.into_iter().collect();
    sc.notes = set.into_iter().collect();
    sc.reason = if sc.parts.is_empty() { no_model_reason(root_path.as_deref().unwrap_or(""), &root.notes) } else { String::new() };
    sc.rows = rows;
    Ok(sc)
}

/// Why an item has nothing to draw, in words for the page (never empty).  Cases found in the archive (EMPIRICAL): the record has
/// no geometry (unreleased items); the record names a file that is not in the archive (LowPoly distance models, a missile whose geometry
/// points at its ship's file); the file is a node-only stub with no mesh file (missile rack / bomb turret mounts: the ship draws them:
/// INFERRED); the file has a mesh but only collision / NoDraw faces.
pub fn no_model_reason(path: &str, notes: &[String]) -> String {
    if path.is_empty() {
        return "The game data gives this item no 3D model (its record has no geometry; usually an unreleased item).".into();
    }
    if notes.iter().any(|n| n.starts_with("model not in the archive")) {
        return format!("The game data points this item at a model file that is not in the archive ({}).", path);
    }
    if notes.iter().any(|n| n.contains("no mesh companion")) {
        return format!("The model file ({}) holds only helper nodes and collision, no mesh: the ship it is mounted on draws the visible part.", path);
    }
    format!("The model file ({}) has no visible faces, only collision or NoDraw geometry.", path)
}

/// JSON-able parts (typed arrays as base64) and the bounding box; each encoded part also carries its `slot`.
pub fn encode_parts(parts: &[ScenePart]) -> (Vec<Value>, Value) {
    let pp: Vec<Part> = parts
        .iter()
        .map(|p| Part {
            name: p.name.clone(),
            kind: "render".into(),
            node: -1,
            verts: p.verts.clone(),
            tris: p.tris.clone(),
            uv: p.uv.clone(),
            vc: p.vc.clone(),
            nrm: p.nrm.clone(),
            tan: p.tan.clone(),
            mat: Some(p.slot as i64),
            ..Default::default()
        })
        .collect();
    let (mut enc, bbox) = encode::encode_parts(&pp, true);
    for (e, p) in enc.iter_mut().zip(parts) {
        e["slot"] = json!(p.slot);
    }
    (enc, bbox)
}

/// `{parts, bbox, slots, notes, placed, tris, reason}`: the `/weapons/<class>/model` payload.
pub fn model_json(sc: &Scene) -> Value {
    let (parts, bbox) = encode_parts(&sc.parts);
    let mut placed = Map::new();
    for r in &sc.rows {
        if !r.cls.is_empty() {
            placed.insert(r.path.clone(), json!(r.m.is_some()));
        }
    }
    json!({"parts": parts, "bbox": bbox, "slots": sc.slots.iter().map(|s| json!({"src": s.src})).collect::<Vec<_>>(), "notes": sc.notes,
        "placed": placed, "tris": sc.parts.iter().map(|p| p.tris.len()).sum::<usize>(), "reason": sc.reason})
}

// ---------------------------------------------------------------------------------------------------------------- materials / skins
fn palette_color(pal: &Option<Value>, idx: i64) -> Option<Vec<f64>> {
    let pal = pal.as_ref()?;
    let entries = pal.get("entries")?.as_array()?;
    if idx < 1 || idx as usize > entries.len() {
        return None;
    }
    entries[idx as usize - 1].get("t")?.as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect())
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// The `look` of a slot source: {palette, material}.
#[derive(Debug, Clone, Default)]
pub struct Look {
    pub palette: Option<String>,
    pub material: Option<String>,
}

fn glow_re() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)glow|emiss|light|lamp|\bled\b|illum|screen|display").unwrap())
}

/// `{materials: [{color, emissive, opacity, alpha_test, tex, name, ...}], textures: [...]}` for `slots` with the tint palette `palette` and, for
/// the 'body' slots, the material file `material` instead of the model's.  NoDraw slots get opacity 0.  `looks` = {slot src: look} for the
/// attachments (a src without an entry is tinted with nothing).  `lazy` (the item pages): textures are not embedded; layered materials carry
/// `layered` and `pal`, plain ones their diffuse map as `tex`.
pub fn slot_materials(p4k: &P4k, items: &ItemMaterials, slots: &[Slot], palette: Option<&str>, material: Option<&str>, with_textures: bool,
                      max_tex: usize, looks: &HashMap<String, Look>, lazy: bool) -> Value {
    let mut reg = MatRegistry::with_options(p4k, max_tex, with_textures, lazy);
    reg.items = Some(items);
    let mut palettes: Vec<Value> = vec![];
    let mut pal_idx: HashMap<Option<String>, usize> = HashMap::new();
    let mut lay = MatRegistry::with_options(p4k, 256, false, false);
    lay.items = Some(items);
    let mut pals: HashMap<Option<String>, Option<Value>> = HashMap::new();
    let mut palette_of = |key: &Option<String>| -> Option<Value> {
        pals.entry(key.clone()).or_insert_with(|| key.as_ref().and_then(|k| items.palette(k))).clone()
    };
    let mut mats: Vec<Value> = vec![];
    for s in slots {
        let mut mtl = s.mtl.clone();
        let body = s.src == "body";
        let look = if body {
            Look { palette: palette.map(str::to_string), material: material.map(str::to_string) }
        } else {
            looks.get(&s.src).cloned().unwrap_or_default()
        };
        let pal = palette_of(&look.palette);
        if let (Some(lm), Some(_)) = (look.material.as_ref().filter(|m| !m.is_empty()), mtl.as_ref()) {
            mtl = Some(strip_mtl(lm));
        }
        let gi = match &mtl {
            Some(m) if !m.is_empty() && s.sub >= 0 => reg.resolve(m, s.sub),
            _ => -1,
        };
        let base: Value = if gi >= 0 {
            reg.materials[gi as usize].clone()
        } else {
            json!({"name": "none", "color": [0.62, 0.66, 0.72], "emissive": [0, 0, 0], "opacity": 1.0, "alpha_test": 0.0, "tex": -1, "nodraw": false})
        };
        let mut m = Map::new();
        for k in ["name", "color", "emissive", "opacity", "alpha_test", "tex"] {
            m.insert(k.into(), base[k].clone());
        }
        if base.get("nodraw") == Some(&Value::Bool(true)) {
            m.insert("opacity".into(), json!(0.0));
        }
        if lazy {
            m.insert("tex".into(), base["tex"].clone());
            if let Some(h) = base.get("hair").filter(|h| truthy(Some(h))) {
                m.insert("hair".into(), h.clone());
            }
            if let Some(lv) = base.get("layered").filter(|l| truthy(Some(l))) {
                let mut l = lv.clone();
                l["wear"] = Value::Null;
                l["pom"] = Value::Null;
                m.insert("layered".into(), l);
                m.insert("tex".into(), json!(-1));
                if !glow_re().is_match(base["name"].as_str().unwrap_or("")) {
                    // EMPIRICAL: only glow-named layered materials keep their emissive colour
                    m.insert("emissive".into(), json!([0.0, 0.0, 0.0]));
                }
                // an item without a tint palette still has palette-tinted layers: the engine's 'default' palette applies
                let dp = match &pal {
                    Some(p) => Some(p.clone()),
                    None => palette_of(&Some(DEFAULT_PALETTE.to_string())),
                };
                if let Some(dp) = dp {
                    let k = if pal.is_some() { look.palette.clone() } else { Some(DEFAULT_PALETTE.to_string()) };
                    let n = palettes.len();
                    let i = *pal_idx.entry(k).or_insert_with(|| {
                        palettes.push(dp);
                        n
                    });
                    m.insert("pal".into(), json!(i));
                }
            }
        }
        let subs = match &mtl {
            Some(p) if !p.is_empty() => lay.load_mtl(p),
            _ => None,
        };
        if lazy && !m.contains_key("layered") {
            if let Some(sm) = subs.as_ref().filter(|_| s.sub >= 0).and_then(|v| v.get(s.sub as usize)) {
                // a plain (Illum) material's TexSlot2 'ddna' normal + gloss map, drawn by the viewer's plain program (EMPIRICAL: 8,154 of 8,800)
                if let Some(nrm) = sm.texture("TexSlot2") {
                    let tail = nrm.to_lowercase();
                    let tail = tail.rsplit('_').next().unwrap_or("").split('.').next().unwrap_or("").to_string();
                    if tail == "ddna" || tail == "ddn" {
                        let t = reg.texture(nrm, "ng");
                        m.insert("nrm".into(), json!(t));
                    }
                }
            }
        }
        if let Some(sm) = subs.as_ref().filter(|_| s.sub >= 0).and_then(|v| v.get(s.sub as usize)) {
            if sm.shader == "ScopePBR" {
                // EMPIRICAL (optc_s02_hdgw_01_mat, the Myers 4x scope): ShaderScopePBR with TexSlot1 '$RenderToTexture'; the %MASK variant is the
                // stencil that cuts the scope picture out.  Drawn as dark, mostly clear glass; the mask is not drawn.
                if sm.gen_mask.split('%').any(|t| t == "MASK") {
                    m.insert("opacity".into(), json!(0.0));
                } else {
                    m.insert("color".into(), json!([0.03, 0.06, 0.09]));
                    m.insert("opacity".into(), json!(0.3));
                }
            }
            // EMPIRICAL (carried-item preview, Sabir mining gadget `display_mat`, 4 vertices): a DisplayScreen / UIPlane quad has no picture
            // here (its image is a render-to-texture the item pages do not run), and its flat Diffuse is white; drawn as a switched-off screen instead.
            if let Some(k) = crate::mfd::rtt_kind(&sm.shader, &sm.textures) {
                if matches!(k, "screen" | "hud") || (k == "static" && base["tex"].as_i64().unwrap_or(-1) < 0) {
                    m.insert("color".into(), json!([0.02, 0.03, 0.04]));
                    m.insert("emissive".into(), json!([0.0, 0.0, 0.0]));
                }
            }
            let bsh = base.get("shader").and_then(Value::as_str).unwrap_or("");
            if DECAL_SHADERS.contains(&bsh) || sm.gen_mask.split('%').any(|t| t == "DECAL") {
                m.insert("decal".into(), json!(true));
                let at = m["alpha_test"].as_f64().unwrap_or(0.0).max(0.35);
                m.insert("alpha_test".into(), json!(at));
            }
            if let Some(md) = base.get("md").filter(|md| truthy(Some(md)) && truthy(md.get("blend"))) {
                // MeshDecal / Illum %DECAL: the viewer shapes the alpha and the glow itself, so the material's own alpha test replaces the cut-out
                m.insert("md".into(), md.clone());
                m.insert("alpha_test".into(), base["alpha_test"].clone());
                m.remove("decal");
                if lazy && truthy(md.get("tm")) && pal.is_some() && !m.contains_key("pal") {
                    // MeshDecal palette tint: the object's own palette only
                    let k = look.palette.clone();
                    let n = palettes.len();
                    let i = *pal_idx.entry(k).or_insert_with(|| {
                        palettes.push(pal.clone().unwrap());
                        n
                    });
                    m.insert("pal".into(), json!(i));
                }
            }
            let lv = lay.layered(sm);
            if let Some(lv) = lv {
                if let Some(l0) = lv["layers"].as_array().and_then(|a| a.first()).filter(|l| truthy(Some(l))) {
                    let mut col: Vec<f64> = l0["tint"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect()).unwrap_or_default();
                    let tint = if truthy(l0.get("pal")) && truthy(l0.get("pd")) { palette_color(&pal, l0["pal"].as_i64().unwrap_or(0)) } else { None };
                    if let Some(t) = tint {
                        for k in 0..col.len().min(3) {
                            col[k] *= t.get(k).copied().unwrap_or(1.0);
                        }
                    }
                    m.insert("color".into(), json!(col.iter().map(|c| rnd(*c, 4)).collect::<Vec<_>>()));
                    if !lazy {
                        m.insert("tex".into(), json!(-1));
                    }
                }
            }
        }
        mats.push(Value::Object(m));
    }
    let mut out = Map::new();
    out.insert("materials".into(), Value::Array(mats));
    out.insert("textures".into(), Value::Array(reg.textures.clone()));
    if lazy {
        out.insert("lazy".into(), json!(true));
        out.insert("palettes".into(), Value::Array(palettes.iter().map(|p| json!({"entries": p["entries"]})).collect()));
    }
    Value::Object(out)
}
