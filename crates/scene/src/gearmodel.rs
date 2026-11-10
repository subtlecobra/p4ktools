//! The 3D model of an armor or clothing item (alone, or worn on the player body) and the colours of its materials for a skin.
//! Evidence notes (EMPIRICAL unless noted):
//!
//! * The worn model is the `Male` (or `Female`) `SubGeometry` node of the record: a `.skin` whose vertices are stored in the bind pose of the
//!   player skeleton (drawn without skinning), a rigid `.cga` (backpacks) or a `.cdf`.
//! * 'Worn' adds the body (`body/m_body.skin` / `f_body`) in the same bind pose, drawn in a flat skin tone; the pose is the skeleton's bind
//!   pose (a T-pose).  A rigid backpack is moved onto the back by `back_frame` (INFERRED and approximate).

use std::collections::HashMap;

use p4k_formats::skeleton;
use p4k::P4k;
use serde_json::{json, Value};

use crate::character;
use crate::dynv::{g, s};
use crate::gear;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::ItemStats;
use crate::weaponmodel::{self as wm, ItemModel, Look, MPart, Mat, Scene, ScenePart, Slot};
use crate::weapons::Weapons;

pub const SKIN_TONE: [f64; 3] = [0.78, 0.60, 0.50];
pub const BACK_ROTATION: Mat = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

pub fn body_file(sex: &str) -> &'static str {
    if sex == "female" {
        "Objects/Characters/Human/female_v2/body/f_body"
    } else {
        "Objects/Characters/Human/male_v7/body/m_body"
    }
}

fn skeleton_file(sex: &str) -> &'static str {
    if sex == "female" {
        "Objects/Characters/Human/female_v2/export/bhf_skeleton_v2.chr"
    } else {
        "Objects/Characters/Human/male_v7/export/bhm_skeleton_v7.chr"
    }
}

fn strip(mtl: &str) -> String {
    let mut m = mtl.replace('\\', "/");
    if m.to_lowercase().starts_with("data/") {
        m = m[5..].to_string();
    }
    if m.to_lowercase().ends_with(".mtl") {
        m.truncate(m.len() - 4);
    }
    m
}

/// The `.mtl` named like the `.skin` (without extension), when the archive has it, else "".
fn skin_default_material(p4k: &P4k, path: &str) -> String {
    let stem = &path[..path.rfind('.').unwrap_or(path.len())];
    if p4k.find_path(&format!("{}.mtl", stem), ".mtl").is_some() {
        stem.to_string()
    } else {
        String::new()
    }
}

/// ItemModel of the worn model `path` (.skin / .cga / .cgf / .cdf) with the material file `material` (may be "").
pub fn load_worn(p4k: &P4k, items: &ItemMaterials, path: &str, material: &str) -> Result<ItemModel, String> {
    if path.to_lowercase().ends_with(".skin") {
        let mut out = ItemModel::default();
        let mut mtl = strip(material);
        if mtl.is_empty() {
            mtl = skin_default_material(p4k, path);
        }
        if mtl.is_empty() {
            out.notes.push(format!("no material file for {}", path));
        }
        let stem = path[..path.rfind('.').unwrap_or(path.len())].to_string();
        out.parts = wm::skin_parts(p4k, path, if mtl.is_empty() { &stem } else { &mtl });
        if out.parts.is_empty() {
            out.notes.push(format!("no mesh (.skinm) for {}", path));
        }
        return Ok(out);
    }
    wm::load_model(p4k, items, path, Some(material).filter(|m| !m.is_empty()))
}

/// Render parts of the player body and head in the bind pose, material key None (drawn in SKIN_TONE).  The head and eyes are the separate skins
/// `character` lists (`head: true`); their vertices share the body's bind space (EMPIRICAL: the head sits on the neck bone).
pub fn body_parts(p4k: &P4k, sex: &str) -> Vec<MPart> {
    let file = body_file(sex);
    let mut parts = wm::skin_parts(p4k, &format!("{}.skin", file), file);
    let kind = character::KINDS.iter().find(|k| k.name == sex).unwrap_or(&character::KINDS[0]);
    for spec in kind.parts {
        if spec.head {
            let path = format!("{}.skin", &spec.skin["Data/".len()..]);
            parts.extend(wm::skin_parts(p4k, &path, &path[..path.len() - 5]));
        }
    }
    for p in parts.iter_mut() {
        p.key = None;
    }
    parts
}

/// 3x4 matrix that carries a backpack model onto the player's back: pack width on x, pack height (its y) up, pack depth (its z, away from the
/// back) towards -y (CryEngine characters face +y), origin at the spine2 bone height and 0.15 m behind the skeleton line.  INFERRED and
/// approximate: the helper the packs really hang on (`backpack_attach_1_override`) is not in the 220-bone skeleton (EMPIRICAL).
pub fn back_frame(frames: &HashMap<String, Mat>) -> Mat {
    let z = frames.get("spine2").map(|m| m[11]).filter(|z| *z != 0.0).unwrap_or(1.275);
    let mut m = BACK_ROTATION;
    m[3] = 0.0;
    m[7] = -0.15;
    m[11] = z;
    m
}

/// {lower-case bone name: 3x4 row-major bind matrix} of the player skeleton.
pub fn skeleton_frames(p4k: &P4k, sex: &str) -> HashMap<String, Mat> {
    let Some(ent) = p4k.find_path(skeleton_file(sex), ".chr") else { return HashMap::new() };
    let Ok(bones) = p4k.read(ent).map_err(|e| e.to_string()).and_then(|b| skeleton::parse_bones(&b).map_err(|e| e.to_string())) else { return HashMap::new() };
    bones.iter().map(|b| (b.name.to_lowercase(), wm::row_major(b.aq, b.ap))).collect()
}

fn move_scene_part(p: &ScenePart, m: &Mat) -> ScenePart {
    let mp = wm::move_part(&MPart { name: p.name.clone(), verts: p.verts.clone(), nrm: p.nrm.clone(), tan: p.tan.clone(), ..Default::default() }, m);
    ScenePart { verts: mp.verts, nrm: mp.nrm, tan: mp.tan, ..p.clone() }
}

/// Why an item has nothing to draw, in words for the page (never empty).  Cases found in the archive (EMPIRICAL): the record has no
/// worn-model node (only the carry box, or nothing); the node names a file that is not in the archive; the file has no mesh companion.
pub fn no_model_reason(path: &str, notes: &[String], entry: Option<&Value>) -> String {
    if path.is_empty() {
        let hold = entry.map_or(false, |e| crate::dynv::truthy(g(e, "hold")));
        return format!("The game data gives this item no worn 3D model (its record only has {}).", if hold { "a carry box" } else { "no geometry; usually an unreleased item" });
    }
    if notes.iter().any(|n| n.contains("no mesh")) {
        return format!("The model file ({}) has no mesh data in the archive.", path);
    }
    if notes.iter().any(|n| n.starts_with("model not in the archive")) {
        return format!("The game data points this item at a model file that is not in the archive ({}).", path);
    }
    format!("The model file ({}) has no visible faces.", path)
}

/// Item class names of an XML default loadout (`<Loadout><Items><Item portName= itemName=><Items>..`), nested items after their parent, in
/// document order.  EMPIRICAL (cds_combat_superheavy_suit_default_01_01_01.xml): the attribute is `itemName`.
pub fn loadout_item_names(data: &[u8]) -> Vec<String> {
    let Ok(root) = p4k_formats::xmlq::parse_any(data) else { return vec![] };
    p4k_formats::xmlq::iter(&root, "Item").into_iter().map(|e| p4k_formats::xmlq::attr(e, "itemName").to_string()).filter(|n| !n.is_empty()).collect()
}

/// Maximum number of default-loadout parts added to one item (a safety bound; the H4 has 2).
const MAX_ATTACHED: usize = 8;

/// The parts the item's default loadout puts on its own `AttachedPart` ports (arms, legs ...) as `(class, model, material, palette key)`.
/// EMPIRICAL (cds_combat_superheavy_suit_01_01_01): the suit record's own model is only the core; `SEntityComponentDefaultLoadoutParams.loadout.
/// loadoutPath` names an XML that lists `cds_combat_superheavy_arms_01_01_01` / `..._legs_...` on ports `Armor_Arms` / `Armor_Legs`; those records
/// have `AttachDef.Type = AttachedPart` and a worn `.skin` of their own (same bind pose as the core).  Only that type is taken, other loadout
/// items (necksock, helmet) are separate wearables.  Manual (inline) loadouts are not read.
pub fn attached_parts(p4k: &P4k, items: &ItemMaterials, stats: &ItemStats, weapons: &Weapons, cls: &str, sex: &str) -> Vec<(String, String, String, Option<String>)> {
    let Some(data) = stats.record(cls, 8) else { return vec![] };
    let path = s(g(g(crate::itemstats::comp(&data, "SEntityComponentDefaultLoadoutParams"), "loadout"), "loadoutPath")).replace('\\', "/");
    if path.is_empty() {
        return vec![];
    }
    let Some(ent) = p4k.find_path(&path, ".xml") else { return vec![] };
    let Ok(bytes) = p4k.read(ent) else { return vec![] };
    let mut out = vec![];
    for name in loadout_item_names(&bytes) {
        let Some(d) = stats.record(&name, 8) else { continue };
        if s(g(g(crate::itemstats::comp(&d, "SAttachableComponentParams"), "AttachDef"), "Type")) != "AttachedPart" {
            continue;
        }
        let Some(lk) = gear::look(items, stats, weapons, p4k, &name, sex, true) else { continue };
        let model = s(&lk["model"]).to_string();
        if model.is_empty() || out.len() >= MAX_ATTACHED {
            continue;
        }
        out.push((name, model, lk["material"].as_str().unwrap_or("").to_string(), lk["palette"].as_str().map(str::to_string)));
    }
    out
}

/// Slot `src` of a default-loadout part: `att:<palette key>` (the palette is that part's own, "" = none).
fn att_src(palette: &Option<String>) -> String {
    format!("att:{}", palette.as_deref().unwrap_or(""))
}

/// The scene of the item `entry` (a gear index entry): parts, slots (src "body" for the item, "human" for the player body), notes, reason.
/// A rigid backpack model is moved onto the back of the player's bind pose; other rigid models stay at their own origin.
pub fn build_scene(p4k: &P4k, items: &ItemMaterials, stats: &ItemStats, weapons: &Weapons, entry: &Value, sex: &str, worn: bool, material: Option<&str>) -> Result<Scene, String> {
    let lk = gear::look(items, stats, weapons, p4k, s(g(entry, "cls")), sex, true);
    let path = lk.as_ref().map(|l| s(&l["model"]).to_string()).unwrap_or_default();
    let mat = material.filter(|m| !m.is_empty()).map(str::to_string).or_else(|| lk.as_ref().and_then(|l| l["material"].as_str().map(str::to_string))).unwrap_or_default();
    let mut sc = Scene::default();
    if path.is_empty() {
        sc.reason = no_model_reason("", &[], Some(entry));
        return Ok(sc);
    }
    let model = load_worn(p4k, items, &path, &mat)?;
    let mut idx: HashMap<(String, Option<(String, i64)>), usize> = HashMap::new();
    let add = |sc: &mut Scene, idx: &mut HashMap<(String, Option<(String, i64)>), usize>, pt: &MPart, src: &str| {
        let slot = *idx.entry((src.to_string(), pt.key.clone())).or_insert_with(|| {
            sc.slots.push(Slot { src: src.to_string(), cls: None, mtl: pt.key.as_ref().map(|k| k.0.clone()), sub: pt.key.as_ref().map_or(-1, |k| k.1) });
            sc.slots.len() - 1
        });
        sc.parts.push(ScenePart { name: pt.name.clone(), verts: pt.verts.clone(), tris: pt.tris.clone(), uv: pt.uv.clone(), slot, nrm: pt.nrm.clone(), tan: pt.tan.clone(), vc: pt.vc.clone() });
    };
    for pt in &model.parts {
        add(&mut sc, &mut idx, pt, "body");
    }
    let mut notes: Vec<String> = model.notes.clone();
    let low = path.to_lowercase();
    if (low.ends_with(".cga") || low.ends_with(".cgf")) && !sc.parts.is_empty() {
        if s(g(entry, "slot")) == "backpack" {
            let frame = back_frame(&skeleton_frames(p4k, sex));
            sc.parts = sc.parts.iter().map(|p| move_scene_part(p, &frame)).collect();
            notes.push("backpack placed on the back by a fixed rule (approximate)".into());
        } else {
            notes.push("rigid model drawn at its own origin".into());
        }
    }
    if !sc.parts.is_empty() {
        for (acls, apath, amat, apal) in attached_parts(p4k, items, stats, weapons, s(g(entry, "cls")), sex) {
            match load_worn(p4k, items, &apath, &amat) {
                Ok(m) => {
                    let src = att_src(&apal);
                    for pt in &m.parts {
                        add(&mut sc, &mut idx, pt, &src);
                    }
                    notes.push(format!("default-loadout part {} added", acls));
                }
                Err(e) => notes.push(format!("default-loadout part {}: {}", acls, e)),
            }
        }
    }
    if worn && !sc.parts.is_empty() {
        for pt in body_parts(p4k, sex) {
            add(&mut sc, &mut idx, &pt, "human");
        }
    }
    let set: std::collections::BTreeSet<String> = notes.into_iter().collect();
    sc.notes = set.into_iter().collect();
    sc.reason = if sc.parts.is_empty() { no_model_reason(&path, &model.notes, Some(entry)) } else { String::new() };
    Ok(sc)
}

/// `/gear/<class>/model`: {parts, bbox, slots, notes, tris, reason, worn}.
pub fn model_json(sc: &Scene, worn: bool) -> Value {
    let (parts, bbox) = wm::encode_parts(&sc.parts);
    json!({"parts": parts, "bbox": bbox, "slots": sc.slots.iter().map(|x| json!({"src": x.src})).collect::<Vec<_>>(), "notes": sc.notes,
        "tris": sc.parts.iter().map(|p| p.tris.len()).sum::<usize>(), "reason": sc.reason, "worn": worn})
}

/// `weaponmodel::slot_materials` for the item's own slots ('body') plus flat skin tone for the human ('human') slots.
///
/// Default-loadout parts (`att:<palette>` slots) keep their own palette; `att_palette` (a chosen skin's palette) replaces it when given.
pub fn slot_materials(p4k: &P4k, items: &ItemMaterials, slots: &[Slot], palette: Option<&str>, att_palette: Option<&str>, material: Option<&str>, with_textures: bool, lazy: bool) -> Value {
    let mapped: Vec<Slot> = slots.iter().map(|x| Slot { src: if x.src == "body" || x.src.starts_with("att:") { x.src.clone() } else { "other".into() }, ..x.clone() }).collect();
    let mut looks = HashMap::new();
    looks.insert("other".to_string(), Look::default());
    for x in slots.iter().filter(|x| x.src.starts_with("att:")) {
        let own = x.src["att:".len()..].to_string();
        let pal = att_palette.map(str::to_string).or(Some(own).filter(|p| !p.is_empty()));
        looks.insert(x.src.clone(), Look { palette: pal, material: None });
    }
    let mut res = wm::slot_materials(p4k, items, &mapped, palette, material, with_textures, 256, &looks, lazy);
    if let Some(mats) = res["materials"].as_array_mut() {
        for (m, sl) in mats.iter_mut().zip(slots) {
            if sl.src == "human" {
                m["color"] = json!(SKIN_TONE);
                m["tex"] = json!(-1);
                m["emissive"] = json!([0, 0, 0]);
                m["opacity"] = json!(1.0);
                m["alpha_test"] = json!(0.0);
                if let Some(o) = m.as_object_mut() {
                    o.remove("decal");
                    o.remove("layered");
                    o.remove("pal");
                }
            }
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadout_items_in_document_order() {
        let xml = br#"<Loadout><Items><Item portName="a" itemName="necksock"/><Item portName="Armor_Arms" itemName="arms_01"><Items><Item portName="x" itemName="glove"/></Items></Item><Item portName="empty"/></Items></Loadout>"#;
        assert_eq!(loadout_item_names(xml), vec!["necksock", "arms_01", "glove"]);
        assert!(loadout_item_names(b"not xml").is_empty());
        assert_eq!(att_src(&None), "att:");
        assert_eq!(att_src(&Some("brand/x".into())), "att:brand/x");
    }
}
