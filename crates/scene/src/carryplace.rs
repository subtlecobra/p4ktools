//! Where a carried item (weapon, grenade, pen, gadget ...) rests on the body: the port helper of the worn host (armor, backpack, body) and the
//! alignment of the item's own model onto it, for the FPS Loadout 3D preview.
//!
//! What the records and files hold (EMPIRICAL on the live archive, read through /dcb and the `.skin` bone chunk):
//!
//! * A `body_attach` item port (`SItemPortDef`, `Flags="body_attach ..."`) has `AttachmentImplementation` of type
//!   `SItemPortDefAttachmentImplementationBone` = `Helper { Tags, Helper { Name, ItemOffsetHelperName, Offset {Rotation, Position},
//!   SimulationParams }, SubHelpers [ { Tags, Helper } ] }`.  Example (thp_light_core_01_01_01): port `wep_stocked_3` -> helper
//!   `wep_stocked_attach_3_override`, item helper `attach_offset_left_01`, sub helper tagged `shouldered`.  The body record's hand ports name the
//!   bones `LeftWeaponBone` / `RightWeaponBone`.
//! * The helper `wep_stocked_attach_3_override` is NOT in the 220-bone `bhm_skeleton_v7.chr`.  It is a bone of the armor's own `.skin`
//!   (`m_thp_light_core_01.skin`: `magazine_attach_N_override`, `gadget_attach_1_override`, `grenade_attach_N_override`,
//!   `wep_stocked_attach_N_override`, `backpack_attach_1_override`, `militaryMultitool_attach_override` ...), whose bone chunk (0xc2011111)
//!   stores each bone in the model space of the bind pose, the space the worn mesh and the body share.  So the host frame is read from the
//!   worn item's skin; the body and its hands from the skeleton.
//! * The item side: `ItemOffsetHelperName` names a helper node in the carried item's own geometry (a weapon `.cga`: `attach_offset_left_01`).
//!
//! CONFIRMED: the item pose is `HostHelperAbs * Offset * inverse(item helper) *
//! storedLocalOffset` with Cry's Hamilton product (QuatT a*b: q = a.q*b.q, t = a.t + rotate(a.q, b.t); b applies first; quaternions x,y,z,w); a missing
//! item helper is the identity (the model origin).  No holster sub-mesh or port-child entity and no other post-attach shift was found (not confirmed absent;
//! left/right mirroring INFERRED absent).  CONFIRMED: the record's Offset holds Euler DEGREES (x,y,z) turned into a quaternion exactly like
//! `Quat_tpl::CreateRotationXYZ` (qz*qy*qx, no sign flip; test `euler_matches_the_engine_quaternion_formula`), position copied as authored.  CONFIRMED: the `SubHelpers` tree is chosen by the port's current tags, see
//! [`PortPlace::select`]; the tags used here are the item's own (`item_tags`) plus the `extra` ones the caller passes (game code adds tags
//! such as shouldered / stocked / pistol / heavyhip, INFERRED).

use std::collections::HashMap;

use p4k_formats::socpak;
use p4k::P4k;
use serde_json::Value;

use crate::dynv::{arr, f, g, s};
use crate::gearmodel;
use crate::itemstats::{comp, ItemStats};
use crate::ships::offset_matrix;
use crate::weaponmodel::{self as wm, Ctx, Mat, Scene, ScenePart, IDENT};

/// One helper reference of a port: the helper (bone) on the host, the helper on the carried item and the offset between them.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperDef {
    pub helper: String,
    pub item_helper: String,
    pub off: Option<Mat>,
}

/// One variant of the `SubHelpers` tree: selected by `tags` (one tag name in the records read), with its own helper and nested variants.
#[derive(Debug, Clone, PartialEq)]
pub struct SubVariant {
    pub tags: String,
    pub def: HelperDef,
    pub subs: Vec<SubVariant>,
}

/// An item port and where it hangs: the main helper and the tree of tagged variants.
#[derive(Debug, Clone, PartialEq)]
pub struct PortPlace {
    pub name: String,
    pub main: HelperDef,
    pub subs: Vec<SubVariant>,
}

impl PortPlace {
    /// The helper that applies for the port's current tags (lower-case words): the deepest matching variant of the `SubHelpers` tree, else the
    /// root.  CONFIRMED: each word of the tag string is hashed case-insensitively and looked up in
    /// the sorted children, then the match recurses; Helper, Offset and ItemOffsetHelperName all come from the one variant.  A node matches when all
    /// its words are in `tags` (the records hold one tag per node; EMPIRICAL).
    pub fn select(&self, tags: &[String]) -> &HelperDef {
        fn pick<'a>(subs: &'a [SubVariant], tags: &[String]) -> Option<&'a HelperDef> {
            let hit = subs.iter().find(|v| {
                let words: Vec<String> = v.tags.to_lowercase().split_whitespace().map(str::to_string).collect();
                !words.is_empty() && words.iter().all(|w| tags.contains(w))
            })?;
            Some(pick(&hit.subs, tags).unwrap_or(&hit.def))
        }
        pick(&self.subs, tags).unwrap_or(&self.main)
    }
}

fn parse_variants(v: &Value) -> Vec<SubVariant> {
    arr(v)
        .iter()
        .filter_map(|x| parse_helper(g(x, "Helper")).map(|def| SubVariant { tags: s(g(x, "Tags")).to_string(), def, subs: parse_variants(g(x, "SubHelpers")) }))
        .collect()
}

/// The tags an item adds to the port it sits in: its `AttachDef.Tags` words, lower-case.  CONFIRMED: attaching an item
/// passes the attachable's tag list to the port, which appends it and re-selects the variant; no record field of the name "itemPortTags"
/// exists, and the list is the weapon record's AttachDef tags as written (the words pistol / stocked / shouldered / heavyhip are the held
/// item's own tags, no game code adds them to a port).  CONFIRMED: a `@tag` goes to the item's own child ports and a `$tag` walks up to a parent port, so only plain words reach this port (those, and
/// words with a `+` depth marker, are skipped here: unmapped).  How a body_attach port gets its INITIAL current tags at load is still open.
pub fn item_tags(stats: &ItemStats, cls: &str) -> Vec<String> {
    let Some(data) = stats.record(cls, 12) else { return vec![] };
    s(g(g(comp(&data, "SAttachableComponentParams"), "AttachDef"), "Tags")).to_lowercase().split_whitespace().filter(|w| !w.starts_with('@') && !w.starts_with('$') && !w.contains('+')).map(str::to_string).collect()
}

fn vec3(v: &Value) -> [f64; 3] {
    [f(g(v, "x")), f(g(v, "y")), f(g(v, "z"))]
}

/// `Helper { Name, ItemOffsetHelperName, Offset }` (the inner struct) as a HelperDef; None without a name.
pub fn parse_helper(h: &Value) -> Option<HelperDef> {
    let name = s(g(h, "Name"));
    if name.is_empty() {
        return None;
    }
    let off = g(h, "Offset");
    let (pos, rot) = (vec3(g(off, "Position")), vec3(g(off, "Rotation")));
    let nz = pos.iter().chain(rot.iter()).any(|v| v.abs() > 1e-6);
    Some(HelperDef { helper: name.to_string(), item_helper: s(g(h, "ItemOffsetHelperName")).to_string(), off: nz.then(|| offset_matrix(pos, rot)) })
}

/// The ports of `cls` that name a bone helper, in record order.
pub fn port_places(stats: &ItemStats, cls: &str) -> Vec<PortPlace> {
    let Some(data) = stats.record(cls, 12) else { return vec![] };
    let mut out = vec![];
    for p in arr(g(comp(&data, "SItemPortContainerComponentParams"), "Ports")) {
        let ai = g(p, "AttachmentImplementation");
        let outer = g(ai, "Helper");
        let Some(main) = parse_helper(g(outer, "Helper")) else { continue };
        let subs = parse_variants(g(outer, "SubHelpers"));
        out.push(PortPlace { name: s(g(p, "Name")).to_string(), main, subs });
    }
    out
}

/// Inverse of a rigid (rotation + translation, uniform or no scale assumed orthonormal) 3x4 matrix; general 3x3 inverse via the adjugate.
pub fn invert(m: &Mat) -> Mat {
    let a = [[m[0], m[1], m[2]], [m[4], m[5], m[6]], [m[8], m[9], m[10]]];
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0]) + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if det.abs() < 1e-12 {
        return IDENT;
    }
    let d = 1.0 / det;
    let inv = [
        [(a[1][1] * a[2][2] - a[1][2] * a[2][1]) * d, (a[0][2] * a[2][1] - a[0][1] * a[2][2]) * d, (a[0][1] * a[1][2] - a[0][2] * a[1][1]) * d],
        [(a[1][2] * a[2][0] - a[1][0] * a[2][2]) * d, (a[0][0] * a[2][2] - a[0][2] * a[2][0]) * d, (a[0][2] * a[1][0] - a[0][0] * a[1][2]) * d],
        [(a[1][0] * a[2][1] - a[1][1] * a[2][0]) * d, (a[0][1] * a[2][0] - a[0][0] * a[2][1]) * d, (a[0][0] * a[1][1] - a[0][1] * a[1][0]) * d],
    ];
    let t = [m[3], m[7], m[11]];
    let mut out = IDENT;
    for i in 0..3 {
        for j in 0..3 {
            out[4 * i + j] = inv[i][j];
        }
        out[4 * i + 3] = -(inv[i][0] * t[0] + inv[i][1] * t[1] + inv[i][2] * t[2]);
    }
    out
}

/// The matrix that carries the item (in its own space) onto the host: `host helper * offset * inverse(item helper)`.
pub fn place_matrix(host_frame: &Mat, off: Option<&Mat>, item_frame: Option<&Mat>) -> Mat {
    let at = match off {
        Some(o) => socpak::compose(host_frame, o),
        None => *host_frame,
    };
    match item_frame {
        Some(i) => socpak::compose(&at, &invert(i)),
        None => at,
    }
}

/// {lower-case helper name: 3x4 bind-pose matrix} of a host in the model space of the player's bind pose.  The skeleton gives the body bones; a worn
/// item adds the bones of its own `.skin` (its `*_override` helpers), a rigid model (backpack) its nodes (moved onto the back when `backpack`).
pub fn host_frames(ctx: &Ctx, cls: &str, sex: &str, is_body: bool, backpack: bool) -> HashMap<String, Mat> {
    let mut out = gearmodel::skeleton_frames(ctx.p4k, sex);
    if is_body {
        out.extend(skin_bones(ctx.p4k, &format!("{}.skin", gearmodel::body_file(sex))));
        return out;
    }
    let Some(look) = crate::gear::look(ctx.items, ctx.stats, ctx.weapons, ctx.p4k, cls, sex, true) else { return out };
    let path = s(&look["model"]).to_string();
    if path.to_lowercase().ends_with(".skin") {
        out.extend(skin_bones(ctx.p4k, &path));
    } else if !path.is_empty() {
        if let Ok(m) = wm::load_model(ctx.p4k, ctx.items, &path, None) {
            let back = gearmodel::back_frame(&out);
            for (k, v) in m.frames {
                out.insert(k, if backpack { socpak::compose(&back, &v) } else { v });
            }
        }
    }
    out
}

/// Bones of a `.skin` as {lower-case name: model-space matrix}.
pub fn skin_bones(p4k: &P4k, path: &str) -> HashMap<String, Mat> {
    let Some(ent) = p4k.find_path(path, ".skin") else { return HashMap::new() };
    let Ok(bytes) = p4k.read(ent) else { return HashMap::new() };
    p4k_formats::skeleton::parse_bones(&bytes).map(|b| b.iter().map(|b| (b.name.to_lowercase(), wm::row_major(b.aq, b.ap))).collect()).unwrap_or_default()
}

/// What `place` found: the matrix (None when the host has no such port helper), and notes for the page.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    pub m: Option<Mat>,
    pub helper: String,
    pub item_helper: String,
    pub notes: Vec<String>,
    /// The three factors of `m` for diagnostics (`/loadout/carried.json?debug=1`): host helper frame, port offset, item helper frame.
    pub parts: Option<(Mat, Option<Mat>, Option<Mat>)>,
}

/// Matrix that puts an item with node frames `item_frames` on port `port` of the host `host_cls`.  `hand_side` ("left" / "right") names a hand
/// port of the body instead (`weapon_attach_hand_<side>`).
pub fn place(ctx: &Ctx, host_cls: &str, port: &str, sex: &str, is_body: bool, backpack: bool, item_frames: &HashMap<String, Mat>, tags: &[String]) -> Placement {
    let mut out = Placement::default();
    let places = port_places(ctx.stats, host_cls);
    let Some(pp) = places.iter().find(|p| p.name.eq_ignore_ascii_case(port)) else {
        out.notes.push(format!("port {} of {} names no bone helper", port, host_cls));
        return out;
    };
    let frames = host_frames(ctx, host_cls, sex, is_body, backpack);
    let hd = pp.select(tags);
    out.helper = hd.helper.clone();
    out.item_helper = hd.item_helper.clone();
    let Some(hf) = frames.get(&hd.helper.to_lowercase()) else {
        out.notes.push(format!("helper {} of port {} is not on the skeleton or the worn model", hd.helper, pp.name));
        return out;
    };
    let item = if hd.item_helper.is_empty() { None } else { item_frames.get(&hd.item_helper.to_lowercase()) };
    if !hd.item_helper.is_empty() && item.is_none() {
        out.notes.push(format!("item helper {} is not in the item model; its origin is used", hd.item_helper));
    }
    out.m = Some(place_matrix(hf, hd.off.as_ref(), item));
    out.parts = Some((*hf, hd.off, item.copied()));
    out
}

/// A scene part with its vertices and tangent frame moved by `m`.
pub fn move_scene_part(p: &ScenePart, m: &Mat) -> ScenePart {
    let mp = wm::move_part(&wm::MPart { name: p.name.clone(), verts: p.verts.clone(), nrm: p.nrm.clone(), tan: p.tan.clone(), ..Default::default() }, m);
    ScenePart { verts: mp.verts, nrm: mp.nrm, tan: mp.tan, ..p.clone() }
}

/// The scene moved by `m`.
pub fn move_scene(sc: &Scene, m: &Mat) -> Scene {
    Scene { parts: sc.parts.iter().map(|p| move_scene_part(p, m)).collect(), ..sc.clone() }
}

/// Scene and materials of a carried item that is not in the weapon index (grenade, pen, gadget, melee ...): the model with its default
/// attachments and the default skin.  Mirrors `WeaponPages::mats` for the root look only.
pub fn generic_scene(ctx: &Ctx, cls: &str) -> Result<(Scene, Value), String> {
    let sc = wm::build_scene(ctx, cls, &HashMap::new())?;
    let mut looks_json: HashMap<String, Value> = HashMap::new();
    looks_json.insert(String::new(), ctx.weapons.item_look(cls, "", None));
    for r in &sc.rows {
        if !r.cls.is_empty() {
            let l = ctx.weapons.item_look(&r.cls, "", looks_json.get(&r.parent));
            looks_json.insert(r.path.clone(), l);
        }
    }
    let looks: HashMap<String, wm::Look> = looks_json
        .iter()
        .map(|(k, v)| (k.clone(), wm::Look { palette: v["palette"].as_str().map(str::to_string), material: v["material"].as_str().map(str::to_string) }))
        .collect();
    let root = &looks_json[""];
    let mats = wm::slot_materials(ctx.p4k, ctx.items, &sc.slots, root["palette"].as_str(), root["material"].as_str(), true, 256, &looks, true);
    Ok((sc, mats))
}

/// The `/loadout/carried.json` model of a scene already moved onto the body: the `/gear/<cls>/model` shape (worn).
pub fn model_json(sc: &Scene) -> Value {
    let (parts, bbox) = wm::encode_parts(&sc.parts);
    serde_json::json!({"parts": parts, "bbox": bbox, "slots": sc.slots.iter().map(|x| serde_json::json!({"src": x.src})).collect::<Vec<_>>(), "notes": sc.notes,
        "tris": sc.parts.iter().map(|p| p.tris.len()).sum::<usize>(), "reason": sc.reason, "worn": true})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn close(a: &Mat, b: &Mat) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn helper_parse() {
        let h = json!({"Name": "wep_stocked_attach_3_override", "ItemOffsetHelperName": "attach_offset_left_01",
            "Offset": {"Rotation": {"x": 0, "y": 0, "z": 0}, "Position": {"x": 0, "y": 0, "z": 0}}});
        let d = parse_helper(&h).unwrap();
        assert_eq!(d.helper, "wep_stocked_attach_3_override");
        assert_eq!(d.item_helper, "attach_offset_left_01");
        assert!(d.off.is_none());
        let h2 = json!({"Name": "x", "Offset": {"Rotation": {"x": 0, "y": 0, "z": 90}, "Position": {"x": 1, "y": 0, "z": 0}}});
        let o = parse_helper(&h2).unwrap().off.unwrap();
        assert!((o[3] - 1.0).abs() < 1e-12 && (o[1] + 1.0).abs() < 1e-9);   // Rz(90): x axis -> y, so the first row's y term is -1
        assert!(parse_helper(&json!({"Name": ""})).is_none());
    }

    fn hd(n: &str) -> HelperDef {
        HelperDef { helper: n.to_string(), item_helper: String::new(), off: None }
    }

    #[test]
    fn variant_selection_deepest_wins() {
        let pp = PortPlace {
            name: "p".into(),
            main: hd("root"),
            subs: vec![
                SubVariant { tags: "Pistol".into(), def: hd("pistol"), subs: vec![] },
                SubVariant { tags: "shouldered".into(), def: hd("sh"), subs: vec![SubVariant { tags: "heavyhip".into(), def: hd("deep"), subs: vec![] }] },
            ],
        };
        let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(pp.select(&t(&["rifle"])).helper, "root");
        assert_eq!(pp.select(&t(&["pistol"])).helper, "pistol");
        assert_eq!(pp.select(&t(&["shouldered"])).helper, "sh");
        assert_eq!(pp.select(&t(&["heavyhip", "shouldered"])).helper, "deep");
        assert_eq!(pp.select(&t(&["heavyhip"])).helper, "root");
    }

    #[test]
    fn invert_roundtrip() {
        let m = wm::row_major([0.2, 0.3, 0.4, 0.8], [1.0, 2.0, 3.0]);
        assert!(close(&socpak::compose(&m, &invert(&m)), &IDENT));
        assert!(close(&socpak::compose(&invert(&m), &m), &IDENT));
    }

    #[test]
    fn placement_aligns_item_helper() {
        // the item helper at (0, 0, 0.1) of the item must land on the host helper
        let host = wm::row_major([0.0, 0.0, 0.0, 1.0], [0.5, 0.2, 1.0]);
        let item_h = wm::row_major([0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.1]);
        let m = place_matrix(&host, None, Some(&item_h));
        let p = p4k_formats::ivo::xform(&m, [0.0, 0.0, 0.1]);
        assert!((p[0] - 0.5).abs() < 1e-9 && (p[1] - 0.2).abs() < 1e-9 && (p[2] - 1.0).abs() < 1e-9);
        // no item helper: the item origin lands on the host helper
        let m2 = place_matrix(&host, None, None);
        assert!(close(&m2, &host));
    }

    #[test]
    fn offset_applies_on_the_host_side() {
        let host = wm::row_major([0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0]);
        let off = wm::row_major([0.0, 0.0, 0.0, 1.0], [0.0, 0.1, 0.0]);
        let m = place_matrix(&host, Some(&off), None);
        assert!((m[7] - 0.1).abs() < 1e-12 && (m[11] - 1.0).abs() < 1e-12);
    }

    /// Row-major 3x4 from local axes (columns, in parent space) and a position.
    fn axes(x: [f64; 3], y: [f64; 3], z: [f64; 3], p: [f64; 3]) -> Mat {
        [x[0], y[0], z[0], p[0], x[1], y[1], z[1], p[1], x[2], y[2], z[2], p[2]]
    }

    #[test]
    fn holstered_pistol_points_down() {
        // EMPIRICAL numbers of gmni_pistol_ballistic_01 on the right thigh holster of srvl_armor_heavy_legs: host helper x back, y outward, z up;
        // item helper (attach_offset_left_01) x = item -z, y = item +x, z = item -y.  Item axes: +y barrel, +z top, +x right side.
        let host = axes([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.256, 0.057, 0.892]);
        let item_h = axes([0.0, 0.0, -1.0], [1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [-0.02, 0.0, 0.027]);
        let m = place_matrix(&host, None, Some(&item_h));
        let dir = |v: [f64; 3]| [m[0] * v[0] + m[1] * v[1] + m[2] * v[2], m[4] * v[0] + m[5] * v[1] + m[6] * v[2], m[8] * v[0] + m[9] * v[1] + m[10] * v[2]];
        let (barrel, top, right) = (dir([0.0, 1.0, 0.0]), dir([0.0, 0.0, 1.0]), dir([1.0, 0.0, 0.0]));
        assert!((barrel[2] + 1.0).abs() < 1e-9, "muzzle down");
        assert!((top[1] - 1.0).abs() < 1e-9, "sights face forward (+y)");
        assert!((right[0] - 1.0).abs() < 1e-9, "right side outward");
        // the item's helper point (its left face) lands on the host helper
        let p = p4k_formats::ivo::xform(&m, [-0.02, 0.0, 0.027]);
        assert!((p[0] - 0.256).abs() < 1e-9 && (p[1] - 0.057).abs() < 1e-9 && (p[2] - 0.892).abs() < 1e-9);
    }

    #[test]
    fn euler_matches_the_engine_quaternion_formula() {
        // the engine's Euler -> quaternion conversion: half angles, x = cz*cy*sx - sz*sy*cx, y = cz*sy*cx + sz*cy*sx, z = sz*cy*cx - cz*sy*sx, w = cy*cx*cz + sy*sx*sz
        let (ax, ay, az) = (31.0f64, -47.0f64, 112.0f64);
        let h = |d: f64| (d.to_radians() * 0.5).sin_cos();
        let ((sx, cx), (sy, cy), (sz, cz)) = (h(ax), h(ay), h(az));
        let (x, y, z, w) = (cz * cy * sx - sz * sy * cx, cz * sy * cx + sz * cy * sx, sz * cy * cx - cz * sy * sx, cy * cx * cz + sy * sx * sz);
        let q = [
            [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
            [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
            [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
        ];
        let m = offset_matrix([0.0; 3], [ax, ay, az]);
        for r in 0..3 {
            for c in 0..3 {
                assert!((m[r * 4 + c] - q[r][c]).abs() < 1e-9, "r{r} c{c}: {} vs {}", m[r * 4 + c], q[r][c]);
            }
        }
    }

    #[test]
    fn euler_offset_order_is_z_y_x() {
        // Rz(90) * Rx(90): x -> y, y -> z, z -> x (columns of the matrix)
        let m = offset_matrix([0.0; 3], [90.0, 0.0, 90.0]);
        let col = |c: usize| [m[c], m[4 + c], m[8 + c]];
        let near = |a: [f64; 3], b: [f64; 3]| a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-9);
        assert!(near(col(0), [0.0, 1.0, 0.0]) && near(col(1), [0.0, 0.0, 1.0]) && near(col(2), [1.0, 0.0, 0.0]));
    }
}
