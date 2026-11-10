//! Reader for `.socpak` object containers (zip files holding CryXmlB entity descriptions).
//!
//! Layout, as observed on Data/ObjectContainers/PU/Wreckage/Space/spacewreck03.socpak (not checked against the engine's container
//! loader):
//!   * a zip whose local-header names use backslashes (most containers) or '/' (some, EMPIRICAL) while the central directory always uses
//!     '/'.  The local header's name is skipped, the central directory is the authority.  Only store (0) and deflate (8) occur (EMPIRICAL: 400 sampled containers);
//!   * `<name>/entdata/<id>.entxml`: one CryXmlB document per entity (root attributes Name, Pos "x,y,z", Rotate "w,x,y,z" (unit
//!     quaternion, identity = 1,0,0,0), optional Scale "sx,sy,sz", EntityClass);
//!   * geometry is referenced by `<... path="objects\\...\\x.cgf"/>` elements; entities of class ObjectContainer reference a nested
//!     container through the objectContainer attribute of EntityComponentObjectContainer;
//!   * `<name>.soc` is a 'CrCh' chunk file.  One chunk (type 4) is an embedded CryXmlB `<Entities>` document with the static entities
//!     (found by its signature).  Chunk 0x10 (render nodes) is decoded by `read_render_nodes`.

use std::collections::{HashMap, HashSet};
use std::io::Read;

use serde_json::{json, Value};

use crate::cryxml::{self, Element};
use crate::error::*;
use crate::mtl::py_float;
use crate::plainxml;

pub const GEOM_EXT: [&str; 2] = [".cgf", ".cga"];
pub const IDENT: [f64; 12] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];

// ------------------------------------------------------------------------------------------------------------------- zip

/// Read access to a socpak zip (store and deflate), members in central-directory order.
pub struct SocZip<'a> {
    data: &'a [u8],
    members: Vec<Member>,
    /// name -> index of the LAST member of that name (a later duplicate wins).
    by_name: HashMap<String, usize>,
}

struct Member {
    name: String,
    method: u16,
    comp: u64,
    size: u64,
    offset: u64,
}

impl<'a> SocZip<'a> {
    pub fn open(data: &'a [u8]) -> Result<SocZip<'a>> {
        let bad = |s: &str| Error::msg(format!("BadZipFile: {}", s));
        let tail_start = data.len().saturating_sub(65_557);
        let eocd = data[tail_start..].windows(4).rposition(|w| w == b"PK\x05\x06").map(|p| p + tail_start).ok_or_else(|| bad("File is not a zip file"))?;
        let mut count = u16_at(data, eocd + 10)? as u64;
        let mut cd_off = u32_at(data, eocd + 16)? as u64;
        if (count == 0xFFFF || cd_off == 0xFFFF_FFFF) && eocd >= 20 && &data[eocd - 20..eocd - 16] == b"PK\x06\x07" {
            let z64 = u64_at(data, eocd - 20 + 8)? as usize;
            if data.get(z64..z64 + 4) != Some(b"PK\x06\x06") {
                return Err(bad("bad zip64 end of central directory"));
            }
            count = u64_at(data, z64 + 32)?;
            cd_off = u64_at(data, z64 + 48)?;
        }
        let mut members = Vec::with_capacity(count as usize);
        let mut p = cd_off as usize;
        for _ in 0..count {
            if data.get(p..p + 4) != Some(b"PK\x01\x02") {
                return Err(bad("Bad magic number for central directory"));
            }
            let method = u16_at(data, p + 10)?;
            let mut comp = u32_at(data, p + 20)? as u64;
            let mut size = u32_at(data, p + 24)? as u64;
            let nlen = u16_at(data, p + 28)? as usize;
            let elen = u16_at(data, p + 30)? as usize;
            let clen = u16_at(data, p + 32)? as usize;
            let mut offset = u32_at(data, p + 42)? as u64;
            let name_b = data.get(p + 46..p + 46 + nlen).ok_or_else(|| bad("truncated central directory"))?;
            let name = String::from_utf8_lossy(name_b).into_owned();
            let extra = data.get(p + 46 + nlen..p + 46 + nlen + elen).ok_or_else(|| bad("truncated central directory"))?;
            let mut q = 0;
            while q + 4 <= extra.len() {
                let id = u16_at(extra, q)?;
                let sz = u16_at(extra, q + 2)? as usize;
                if id == 1 {
                    let mut f = q + 4;
                    // zip64 fields appear only for the 32-bit fields that are 0xFFFFFFFF, in this order
                    if size == 0xFFFF_FFFF {
                        size = u64_at(extra, f)?;
                        f += 8;
                    }
                    if comp == 0xFFFF_FFFF {
                        comp = u64_at(extra, f)?;
                        f += 8;
                    }
                    if offset == 0xFFFF_FFFF {
                        offset = u64_at(extra, f)?;
                    }
                }
                q += 4 + sz;
            }
            members.push(Member { name, method, comp, size, offset });
            p += 46 + nlen + elen + clen;
        }
        let by_name = members.iter().enumerate().map(|(i, m)| (m.name.clone(), i)).collect();
        Ok(SocZip { data, members, by_name })
    }

    pub fn namelist(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(|m| m.name.as_str())
    }

    /// (name, uncompressed size) of every member, in central-directory order.
    pub fn sizes(&self) -> impl Iterator<Item = (&str, u64)> {
        self.members.iter().map(|m| (m.name.as_str(), m.size))
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        let m = &self.members[*self.by_name.get(name).ok_or_else(|| Error::msg(format!("KeyError: {}", name)))?];
        let h = m.offset as usize;
        if self.data.get(h..h + 4) != Some(b"PK\x03\x04") {
            return Err(Error::msg("BadZipFile: Bad magic number for file header"));
        }
        let start = h + 30 + u16_at(self.data, h + 26)? as usize + u16_at(self.data, h + 28)? as usize;
        let raw = self.data.get(start..start + m.comp as usize).ok_or_else(|| Error::msg("BadZipFile: truncated member"))?;
        match m.method {
            0 => Ok(raw.to_vec()),
            8 => {
                let mut out = Vec::with_capacity(m.size as usize);
                flate2::read::DeflateDecoder::new(raw).read_to_end(&mut out).map_err(|e| Error::msg(format!("zlib.error: {}", e)))?;
                Ok(out)
            }
            x => Err(Error::msg(format!("NotImplementedError: compression type {}", x))),
        }
    }
}

// ------------------------------------------------------------------------------------------------------------------- helpers

/// Exactly N comma separated floats, else None.
fn floats<const N: usize>(s: Option<&str>) -> Option<[f64; N]> {
    let v: Option<Vec<f64>> = s?.split(',').map(py_float).collect();
    let v = v?;
    if v.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    out.copy_from_slice(&v);
    Some(out)
}

fn floats_or<const N: usize>(s: Option<&str>, default: [f64; N]) -> [f64; N] {
    floats(s).unwrap_or(default)
}

/// Float of an attribute, None when absent or malformed.
fn fl(el: &Element, name: &str) -> Option<f64> {
    el.get(name).and_then(py_float)
}

fn fl_or(el: &Element, name: &str, default: f64) -> f64 {
    fl(el, name).unwrap_or(default)
}

/// `el.get(name)` when non-empty.
fn get_t<'a>(el: &'a Element, name: &str) -> Option<&'a str> {
    el.get(name).filter(|s| !s.is_empty())
}

/// All descendants including `el`, document order (ElementTree `iter()`).
pub fn iter_all(el: &Element) -> Vec<&Element> {
    fn go<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
        out.push(e);
        for c in &e.children {
            go(c, out);
        }
    }
    let mut out = vec![];
    go(el, &mut out);
    out
}

/// `el.find(".//tag")`: first descendant (not `el` itself) with the tag.
fn find_desc<'a>(el: &'a Element, tag: &str) -> Option<&'a Element> {
    iter_all(el).into_iter().skip(1).find(|e| e.tag == tag)
}

/// `el.find(".//a/b")`.
fn find_desc_child<'a>(el: &'a Element, a: &str, b: &str) -> Option<&'a Element> {
    iter_all(el).into_iter().skip(1).filter(|e| e.tag == a).find_map(|e| e.find(b))
}

fn vec3(a: [f64; 3]) -> Value {
    json!([a[0], a[1], a[2]])
}

/// 3x4 row-major local->parent matrix from a (w,x,y,z) quaternion, per-axis scale and position.
pub fn quat_scale_pos_matrix(q: [f64; 4], scale: [f64; 3], pos: [f64; 3]) -> [f64; 12] {
    let [mut w, mut x, mut y, mut z] = q;
    let mut n = (w * w + x * x + y * y + z * z).sqrt();
    if n == 0.0 {
        n = 1.0;
    }
    w /= n;
    x /= n;
    y /= n;
    z /= n;
    let r = [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
    ];
    [
        r[0][0] * scale[0], r[0][1] * scale[1], r[0][2] * scale[2], pos[0],
        r[1][0] * scale[0], r[1][1] * scale[1], r[1][2] * scale[2], pos[1],
        r[2][0] * scale[0], r[2][1] * scale[1], r[2][2] * scale[2], pos[2],
    ]
}

/// a * b for 3x4 row-major affine matrices (apply b first, then a).
pub fn compose(a: &[f64; 12], b: &[f64; 12]) -> [f64; 12] {
    let mut out = [0.0; 12];
    for i in 0..3 {
        for j in 0..3 {
            out[4 * i + j] = a[4 * i] * b[j] + a[4 * i + 1] * b[4 + j] + a[4 * i + 2] * b[8 + j];
        }
        out[4 * i + 3] = a[4 * i] * b[3] + a[4 * i + 1] * b[7] + a[4 * i + 2] * b[11] + a[4 * i + 3];
    }
    out
}

// ------------------------------------------------------------------------------------------------------------------- entity info

/// Editor-relevant parameters of a light entity (as JSON), or None for any other class.
///
/// Classes: Light (component EntityComponentLight with lightType Projector / Omni / SoftOmni / Planar / Ambient, or none for star-like
/// lights), LightBox (BBox_os Min/Max) and EnvironmentLight.  EMPIRICAL (300 sampled containers): a Light points along its local +X
/// axis.  CONFIRMED against the engine's light render node: the light points along local +X; FOV
/// is the FULL cone angle in degrees; a Planar light spans planeWidth along local Y and planeHeight along local Z (normal +X); ELightType
/// is Omni, SoftOmni, Projector, Planar, Ambient only (a missing lightType is the default Omni); the plane size looks like a
/// half-extent (INFERRED).  NOT modelled: styleParams.angularSpeed spinners and named lightanimation tracks.
pub fn light_info(root: &Element) -> Option<Value> {
    match root.get("EntityClass") {
        Some("Light") => {
            let mut out = json!({"kind": "light", "guid": root.get("EntityCryGUID"), "type": "None", "radius": fl_or(root, "Radius", 1.0), "bulb": 0.0,
                "color": null, "temp": null, "intensity": 1.0, "fov": 90.0, "plane": [1.0, 1.0], "active": true, "flare": false});
            let Some(comp) = find_desc(root, "EntityComponentLight") else { return Some(out) };
            out["type"] = json!(get_t(comp, "lightType").unwrap_or("None"));
            out["active"] = json!(comp.get("active") != Some("0"));
            if let Some(size) = comp.find("sizeParams") {
                out["radius"] = json!(fl_or(size, "lightRadius", out["radius"].as_f64().unwrap_or(1.0)));
                out["bulb"] = json!(fl_or(size, "bulbRadius", 0.0));
                out["plane"] = json!([fl_or(size, "planeWidth", 1.0), fl_or(size, "planeHeight", 1.0)]);
            }
            if let Some(st) = comp.find("defaultState") {
                out["intensity"] = json!(fl_or(st, "intensity", 1.0));
                if let Some(c) = st.find("color") {
                    out["color"] = json!([fl_or(c, "r", 0.0), fl_or(c, "g", 0.0), fl_or(c, "b", 0.0)]);
                } else if comp.get("useTemperature") == Some("1") && get_t(st, "temperature").is_some() {
                    out["temp"] = json!(fl(st, "temperature"));
                }
            }
            if let Some(pr) = comp.find("projectorParams") {
                out["fov"] = json!(fl_or(pr, "FOV", 90.0));
            }
            out["flare"] = json!(comp.find("flareParams").map_or(false, |f| f.get("flareEnabled") == Some("1")));
            Some(out)
        }
        Some("LightBox") => {
            let b = find_desc(root, "BBox_os")?;
            let (lo, hi) = (floats::<3>(b.get("Min"))?, floats::<3>(b.get("Max"))?);
            Some(json!({"kind": "lightbox", "box": [vec3(lo), vec3(hi)]}))
        }
        Some("EnvironmentLight") => Some(json!({"kind": "envlight", "radius": fl_or(root, "Radius", 1.0)})),
        _ => None,
    }
}

/// 3x4 matrix of a Segment transform_ls string 'Q : w,x,y,z | T : x,y,z' (identity when absent or malformed).
fn transform_ls(s: &str) -> [f64; 12] {
    let parse = || -> Option<[f64; 12]> {
        let mut it = s.split('|');
        let (q, t) = (it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        let q = q.split(':').nth(1)?.trim();
        let t = t.split(':').nth(1)?.trim();
        Some(quat_scale_pos_matrix(floats_or(Some(q), [1.0, 0.0, 0.0, 0.0]), [1.0; 3], floats_or(Some(t), [0.0; 3])))
    };
    parse().unwrap_or(IDENT)
}

fn points(parent: Option<&Element>, attr: &str, tag: Option<&str>) -> Vec<Value> {
    let Some(p) = parent else { return vec![] };
    let mut v = vec![];
    match tag {
        None => {
            for e in &p.children {
                if let Some(q) = floats::<3>(e.get(attr)) {
                    v.push(vec3(q));
                }
            }
        }
        Some(t) => {
            for e in iter_all(p).into_iter().filter(|e| e.tag == t) {
                if let Some(q) = floats::<3>(e.get(attr)) {
                    v.push(vec3(q));
                }
            }
        }
    }
    v
}

/// Editor volume of an AreaBox / AreaShape / Room / ... entity as JSON, or None.
///
/// EMPIRICAL (300 sampled containers, 407 AreaBox, 266 AreaShape, 74 Room): AreaBox is one Segment of Type=Box with BoxMin / BoxMax in
/// the segment's local space; AreaShape a Segment Type=Shape with Points (Pos, z = 0) and Height (the prism rises along +Z, UNCONFIRMED);
/// NavigationArea has the AreaShape component shape; ActionArea only a bounding Radius; FogVolume a box; Room has no stored volume
/// (sentinel MinAABB 1e15), so a sphere of its Radius is all that can be shown.  Other classes:
/// GenericShape, LandingArea/HangarArea INFERRED full size, NavSpline control polygon, FogVolume full extent, RoomConnector full size
/// with the Radius inferring a missing component, LedgeObject, AICoverSurface, markers.
pub fn volume_info(root: &Element) -> Option<Value> {
    let cls = root.get("EntityClass")?;
    match cls {
        "AreaBox" | "AreaShape" | "NavigationArea" => {
            let comp = if cls == "AreaBox" { root.find("AreaBoxComponent") } else { root.find("AreaComponent") };
            let seg = comp?.find("Segment")?;
            let local = transform_ls(seg.get("transform_ls").unwrap_or(""));
            if cls == "AreaBox" && seg.get("Type") == Some("Box") {
                let (lo, hi) = (floats::<3>(seg.get("BoxMin"))?, floats::<3>(seg.get("BoxMax"))?);
                return Some(json!({"kind": "areabox", "box": [vec3(lo), vec3(hi)], "local": local}));
            }
            if cls != "AreaBox" && seg.get("Type") == Some("Shape") {
                let pts = points(seg.find("Points"), "Pos", None);
                let height = get_t(seg, "Height").and_then(py_float).unwrap_or(0.0);
                return if pts.len() >= 3 {
                    Some(json!({"kind": if cls == "AreaShape" { "areashape" } else { "navarea" }, "pts": pts, "height": height, "local": local}))
                } else {
                    None
                };
            }
            None
        }
        "GenericShape" => {
            // EMPIRICAL (asteroid_base/tsg/idris/base_int_hangar_main, 15 entities): the same AreaComponent / Segment as AreaBox and
            // AreaShape, stored twice in the entity (identical).
            let seg = root.find("AreaComponent")?.find("Segment")?;
            let local = transform_ls(seg.get("transform_ls").unwrap_or(""));
            if seg.get("Type") == Some("Box") {
                let (lo, hi) = (floats::<3>(seg.get("BoxMin"))?, floats::<3>(seg.get("BoxMax"))?);
                return Some(json!({"kind": "areabox", "box": [vec3(lo), vec3(hi)], "local": local}));
            }
            let pts = points(seg.find("Points"), "Pos", None);
            if pts.len() >= 3 {
                Some(json!({"kind": "areashape", "pts": pts, "height": fl(seg, "Height").unwrap_or(0.0), "local": local}))
            } else {
                None
            }
        }
        "LandingArea" | "HangarArea" => {
            // INFERRED: the component's dimensions are the FULL size of a box centred on the entity.
            if let Some(d) = find_desc_child(root, "LandingArea", "dimensions") {
                return Some(json!({"kind": "sizedbox", "size": [fl_or(d, "x", 1.0), fl_or(d, "y", 1.0), fl_or(d, "z", 1.0)]}));
            }
            let c = find_desc(root, "EntityComponentHangarArea")?;
            Some(json!({"kind": "sizedbox", "size": [fl_or(c, "xDimension", 1.0), fl_or(c, "yDimension", 1.0), fl_or(c, "zDimension", 1.0)]}))
        }
        "NavSpline" | "LandingSpline" | "TransitNavSpline" => {
            // EMPIRICAL: <NavSpline><ControlPoint position="x,y,z" .../> in entity space; the control polygon is drawn.
            let pts = points(Some(root), "position", Some("ControlPoint"));
            if pts.len() >= 2 {
                Some(json!({"kind": "spline", "pts": pts}))
            } else {
                None
            }
        }
        "Room" | "ActionArea" => Some(json!({"kind": if cls == "Room" { "room" } else { "actionarea" }, "radius": fl_or(root, "Radius", 0.0)})),
        "FogVolume" => {
            // EMPIRICAL: FogVolumeComponentParams/size is the FULL extent; volumeType is unknown, every fog volume is a box.
            let size = find_desc_child(root, "FogVolume", "size").or_else(|| find_desc(root, "size"))?;
            Some(json!({"kind": "fog", "size": [fl_or(size, "x", 1.0), fl_or(size, "y", 1.0), fl_or(size, "z", 1.0)]}))
        }
        "RoomConnector" => {
            // EMPIRICAL: defaultAreaBounds x/y/z is the FULL size of the connector box (the entity Radius is half its diagonal); a
            // missing component is whatever the Radius implies.
            let b = find_desc(root, "defaultAreaBounds")?;
            let mut size: Vec<Option<f64>> = ["x", "y", "z"].iter().map(|k| fl(b, k)).collect();
            let known: f64 = size.iter().flatten().map(|v| v * v).sum();
            let rad = fl_or(root, "Radius", 0.0);
            let miss = size.iter().filter(|v| v.is_none()).count();
            let fill = if rad != 0.0 { (((2.0 * rad) * (2.0 * rad) - known).max(0.0) / miss.max(1) as f64).powf(0.5) } else { 1.0 };
            for v in size.iter_mut() {
                if v.is_none() {
                    *v = Some(fill);
                }
            }
            let off = find_desc(root, "boundsOffset").map_or([0.0; 3], |o| [fl_or(o, "x", 0.0), fl_or(o, "y", 0.0), fl_or(o, "z", 0.0)]);
            Some(json!({"kind": "roomconnector", "size": size, "offset": off}))
        }
        "LedgeObject" => {
            let gv = root.find("GameVolumeComponent");
            let pts = points(gv, "Pos", Some("Point"));
            if pts.len() >= 2 {
                Some(json!({"kind": "ledge", "pts": pts, "height": gv.map(|g| fl_or(g, "volumeHeight", 1.0))}))
            } else {
                None
            }
        }
        "AICoverSurface" => {
            let mut samples = vec![];
            for e in iter_all(root).into_iter().filter(|e| e.tag == "Sample") {
                if let Some(p) = floats::<3>(e.get("Pos")) {
                    samples.push(json!([p[0], p[1], p[2], fl_or(e, "Height", 1.0)]));
                }
            }
            if samples.len() >= 2 {
                samples.truncate(400);
                Some(json!({"kind": "cover", "samples": samples}))
            } else {
                None
            }
        }
        "ParticleEffect" | "HarvestableTagPoint" | "AudioTriggerSpot" | "TagPoint" => Some(json!({"kind": "marker", "cls": cls, "members": []})),
        "ColorGradient" | "SitOnFLoor" | "LightGroup" => {
            let members: Vec<&str> = iter_all(root)
                .into_iter()
                .filter(|e| e.tag == "Light" || e.tag == "EnvironmentProbe")
                .filter_map(|e| get_t(e, "EntityCryGUID"))
                .collect();
            Some(json!({"kind": "marker", "cls": cls, "members": members}))
        }
        _ => None,
    }
}

/// GUIDs an entity links to: `<EntityLinks><Link TargetCryGUID=...>`.  EMPIRICAL (Star_Runner base_int_body_main): a wall panel links to its
/// door's proximity sensor and to the door itself; link names are all 'NewLink', so meaning is read from the class at the other end.
pub fn entity_links(root: &Element) -> Vec<String> {
    let Some(l) = root.find("EntityLinks") else { return vec![] };
    l.children.iter().filter(|c| c.tag == "Link").filter_map(|c| get_t(c, "TargetCryGUID")).map(str::to_owned).collect()
}

/// Ladder parameters (`{'height', 'approach_top', 'box'}`) or None without a `LadderComponentParams` element.  EMPIRICAL (888 `Ladder`
/// entities of 422 containers): only fields the designer changed are present; the entity's AreaComponent box is the trigger volume.
pub fn ladder_info(root: &Element) -> Option<Value> {
    let el = iter_all(root).into_iter().find(|e| e.get("__type") == Some("LadderComponentParams"))?;
    let seg = root.find("AreaComponent").and_then(|a| a.find("Segment"));
    let bx = match seg {
        Some(s) if get_t(s, "BoxMin").is_some() => {
            let g = |k| floats::<3>(s.get(k)).map_or(Value::Null, vec3);
            json!([g("BoxMin"), g("BoxMax")])
        }
        _ => Value::Null,
    };
    let f = |k: &str| get_t(el, k).and_then(py_float);
    Some(json!({"height": f("height"), "approach_top": f("approachAngleAtTop"), "box": bx}))
}

/// An entity hung on a helper node of another entity.  EMPIRICAL: `ParentEntityCryGUID`,
/// `AttachmentType="CharacterBone"`, `AttachmentTarget="$slot_sign_01"`; Pos / Rotate are already the placement in the container.
#[derive(Debug, Clone, PartialEq)]
pub struct Attach {
    pub parent: String,
    pub target: String,
}

pub fn entity_attachment(root: &Element) -> Option<Attach> {
    let (parent, target) = (get_t(root, "ParentEntityCryGUID")?, get_t(root, "AttachmentTarget")?);
    if root.get("AttachmentType") == Some("CharacterBone") {
        Some(Attach { parent: parent.to_lowercase(), target: target.to_string() })
    } else {
        None
    }
}

/// `[(path, trigger)]` of every `audioTrigger="..."` attribute inside the PropertiesDataCore of an entity element.  EMPIRICAL
/// (Constellation, Redeemer containers): empty values are common and skipped.
pub fn entity_audio(root: &Element) -> Vec<(String, String)> {
    fn walk(el: &Element, path: &str, out: &mut Vec<(String, String)>) {
        if let Some(t) = get_t(el, "audioTrigger") {
            out.push((format!("{}{}", path, el.tag), t.to_string()));
        }
        let np = format!("{}{}.", path, el.tag);
        for c in &el.children {
            walk(c, &np, out);
        }
    }
    let mut out = vec![];
    for c in &root.children {
        if c.tag == "PropertiesDataCore" {
            for cc in &c.children {
                walk(cc, "", &mut out);
            }
        }
    }
    out
}

/// One placed thing of a container.  Covers the entities of the entdata documents, of render nodes (`mtl`, `pal`, `vdr`), of
/// decals (`decal`) and of the palette table (`palettes`), and of metadata children (`guid`, `container`).
#[derive(Debug, Clone, Default)]
pub struct Entity {
    pub name: String,
    pub cls: Option<String>,
    pub attach: Option<Attach>,
    pub matrix: [f64; 12],
    /// lowercase, forward slashes
    pub geometry: Option<String>,
    pub container: Option<String>,
    pub light: Option<Value>,
    pub volume: Option<Value>,
    pub guid: Option<String>,
    pub links: Vec<String>,
    pub ladder: Option<Value>,
    /// `transit::manager_info(root)` or `transit::destination_info(root)` ; elevator / tram stops
    pub transit: Option<Value>,
    pub audio: Vec<(String, String)>,
    /// material path of a decal node
    pub decal: Option<String>,
    pub mtl: Option<String>,
    pub pal: Option<String>,
    /// view-distance ratio byte of render nodes and decals
    pub vdr: Option<u8>,
    /// non-empty only for the palette-table pseudo entity
    pub palettes: Vec<String>,
}

fn entity_from_root(root: &Element, fallback_name: &str) -> Entity {
    let pos = floats_or(root.get("Pos"), [0.0; 3]);
    let rot = floats_or(root.get("Rotate"), [1.0, 0.0, 0.0, 0.0]);
    let sc = floats_or(root.get("Scale"), [1.0; 3]);
    let (mut geom, mut container) = (None, None);
    for el in iter_all(root) {
        if geom.is_none() {
            if let Some(p) = get_t(el, "path") {
                if GEOM_EXT.iter().any(|e| p.to_lowercase().ends_with(e)) {
                    geom = Some(p.replace('\\', "/").to_lowercase());
                }
            }
        }
        if container.is_none() {
            if let Some(oc) = get_t(el, "objectContainer") {
                if oc.to_lowercase().ends_with(".socpak") {
                    container = Some(oc.replace('\\', "/").to_lowercase());
                }
            }
        }
    }
    Entity {
        name: get_t(root, "Name").unwrap_or(fallback_name).to_string(),
        cls: root.get("EntityClass").map(str::to_owned),
        attach: entity_attachment(root),
        matrix: quat_scale_pos_matrix(rot, sc, pos),
        geometry: geom,
        container,
        light: light_info(root),
        volume: volume_info(root),
        guid: root.get("EntityCryGUID").map(str::to_owned),
        links: entity_links(root),
        ladder: ladder_info(root),
        transit: crate::transit::manager_info(root).or_else(|| crate::transit::destination_info(root)),
        audio: entity_audio(root),
        ..Default::default()
    }
}

/// An `<Entity>` carrying nothing but EntityCryGUID.  EMPIRICAL (145 containers under asd_layout_facility_001): 1,482 of 1,482 such
/// elements repeat the GUID of a full record in the same .soc, so they carry no geometry or position of their own.
pub fn is_bare_reference(el: &Element) -> bool {
    el.attrs.iter().all(|(k, _)| k == "EntityCryGUID") && el.children.is_empty()
}

/// Static entities: the .soc embeds one CryXmlB `<Entities>` document (found by its signature).  Returns (entities, errors).
pub fn read_soc_static_entities(soc: &[u8]) -> (Vec<Entity>, Vec<String>) {
    let Some(o) = soc.windows(7).position(|w| w == b"CryXmlB") else { return (vec![], vec![]) };
    match cryxml::parse(&soc[o..]) {
        Err(e) => (vec![], vec![format!("soc Entities: {}", e)]),
        Ok(root) => {
            let mut v = vec![];
            for el in iter_all(&root).into_iter().filter(|e| e.tag == "Entity") {
                if !is_bare_reference(el) {
                    v.push(entity_from_root(el, "static"));
                }
            }
            (v, vec![])
        }
    }
}

// ------------------------------------------------------------------------------------------------------------------- .soc chunks

pub const RENDER_NODE_CHUNK: u16 = 0x10;
const NODE_BRUSH: u32 = 1;
const NODE_DECAL: u32 = 0x10;
const NODE_9: u32 = 9;
const NODE_COMPOUND: u32 = 0x1A;
const NODE_16: u32 = 0x16;
const NO_INDEX: u16 = 0xFFFF;
const DECAL_STRIDE: usize = 0x88;
const COMPOUND_STRIDE: usize = 0xBC;
const NODE16_STRIDE: usize = 0xA4;

/// Brush node size by header version (the engine's loader hard-codes the sizes).
fn brush_stride(version: u32) -> Option<usize> {
    match version {
        8..=11 => Some(0xB0),
        12 => Some(0xB8),
        13 => Some(0xC0),
        14 => Some(0xC8),
        15 => Some(0xD0),
        _ => None,
    }
}

pub struct Chunk<'a> {
    pub ctype: u16,
    pub version: u16,
    pub id: u32,
    pub data: &'a [u8],
}

/// Chunk table of a .soc.  CONFIRMED against the engine's chunk file reader: 'CrCh', file version 0x746,
/// 16-byte entries {u16 type, u16 ver, u32 id, u32 size, u32 off}.
pub fn soc_chunks(soc: &[u8]) -> Result<Vec<Chunk<'_>>> {
    if soc.get(..4) != Some(b"CrCh") {
        return Ok(vec![]);
    }
    let count = u32_at(soc, 8)? as usize;
    let table = u32_at(soc, 12)? as usize;
    let mut out = vec![];
    for i in 0..count {
        let o = table + 16 * i;
        let size = u32_at(soc, o + 8)? as usize;
        let off = u32_at(soc, o + 12)? as usize;
        let lo = off.min(soc.len());
        let hi = off.saturating_add(size).min(soc.len());
        out.push(Chunk { ctype: u16_at(soc, o)?, version: u16_at(soc, o + 2)?, id: u32_at(soc, o + 4)?, data: &soc[lo..hi] });
    }
    Ok(out)
}

/// NUL-terminated latin-1 string from the (clamped) slice `b[start..start+256]`.
fn cstr(b: &[u8], start: usize, len: usize) -> String {
    let lo = start.min(b.len());
    let s = &b[lo..start.saturating_add(len).min(b.len())];
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    s[..end].iter().map(|&c| c as char).collect()
}

fn node_size(chunk: &[u8], pos: usize, ntype: u32, version: u32) -> Result<Option<usize>> {
    Ok(match ntype {
        NODE_BRUSH => brush_stride(version),
        NODE_DECAL => Some(DECAL_STRIDE),
        NODE_COMPOUND => Some(COMPOUND_STRIDE),
        NODE_16 => Some(NODE16_STRIDE),
        NODE_9 => {
            if version < 10 {
                Some(0xB8)
            } else {
                let n1 = u32_at(chunk, pos + 0xD0)? as i64;
                let n2 = u32_at(chunk, pos + 0xDC)? as i64;
                let flag = u8_at(chunk, pos + 0x43)? as i8 as i64;
                let s = 0xE0 + flag * 4 + n1 * 12 + n2 * 12;
                if s > 0 {
                    Some(s as usize)
                } else {
                    None
                }
            }
        }
        _ => None,
    })
}

/// Tint palette paths in the header of a chunk 0x10 (the table after the material paths: packed>>16 entries of 256 bytes).  EMPIRICAL:
/// 178 of 250 sampled containers list some, most of them 'Libs/Foundry/Records/TintPalettes/default'.  Which node uses which entry is
/// NOT decoded here.
pub fn render_node_palettes(chunk: &[u8]) -> Vec<String> {
    let f = || -> Result<Vec<String>> {
        let n1 = u32_at(chunk, 0)? as usize;
        let mut pos = 4 + n1 * 0x15C;
        let n2 = u32_at(chunk, pos)? as usize;
        pos += 4 + 256 * n2;
        let packed = u32_at(chunk, pos)? as usize;
        pos += 4 + 256 * (packed & 0xFFFF);
        // the header must be complete; the strings themselves are clamped slices
        Ok((0..packed >> 16).map(|i| cstr(chunk, pos + 256 * i, 256)).collect())
    };
    f().unwrap_or_default()
}

/// Decode a .soc chunk 0x10 (version 1) into placed brush nodes.  Returns (entities, notes).
///
/// Chunk layout CONFIRMED against the engine's render node loader: u32 N1 + N1*0x15c legacy table; u32 N2 + N2*256
/// geometry paths; u32 (low16 nMaterials, high16 nTintPalettes); material paths (256 each); tint-palette paths (256 each); node header
/// 0x20 bytes {u32 version, 3xf32 min, 3xf32 max, u32 dataSize}; then nodes, each starting with u32 nodeType.  Node sizes are hard-coded
/// by type and header version in the loader, there is no size field, so an unknown
/// type aborts the chunk.  Offsets from the node start (EMPIRICAL on every Data/ObjectContainers .socpak: all 9668 chunks end exactly at
/// dataSize; versions 8..15 occur):
///   * brush (1): geometry index u16 at +0x3c (0xffff = none), row-major 3x4 f64 matrix at +0x40 with the scale baked in, relative to the
///     container (matrix and composition confirmed in code); size by version;
///   * compound (0x1a, v14+): 0xbc bytes then (u16 at +0xa2) + 1 complete child nodes, already in container space; they are the nodes
///     that follow in the stream, so a sequential walk flattens them;
///   * decal (0x10, 0x88 bytes): quaternion x,y,z,w as 4 f64 at +0x40, position 3 f64 at +0x60, uniform scale f64 at +0x78, u16 index into
///     the chunk's material path table at +0x80 (EMPIRICAL: valid on all 24,453 decals of 500 sampled containers);
///   * view distance (CONFIRMED location): the decal node carries a per-node viewDistRatio at record +0x3e, read here as the u8 at
///     +0x3e of decals AND brushes (`vdr`); culling by it is NOT implemented;
///   * per-node material override and tint palette (CONFIRMED in the loader): s16 material index at +0xaa (header
///     version >= 12) / +0xa4 (<= 11), u16 palette index at +0xac / +0xa6; negative / 0xffff = none (EMPIRICAL: 110,000 brush nodes);
///   * types 9 and 0x16: sized and skipped, not drawn.
/// Header versions 5..7 never occur and are not decoded.
pub fn read_render_nodes(chunk: &[u8]) -> (Vec<Entity>, Vec<String>) {
    let mut notes = vec![];
    let mut ents = vec![];
    let r = (|| -> Result<()> {
        let n1 = u32_at(chunk, 0)? as usize;
        let mut pos = 4 + n1 * 0x15C;
        let n2 = u32_at(chunk, pos)? as usize;
        pos += 4;
        let geoms: Vec<String> = (0..n2).map(|i| cstr(chunk, pos + 256 * i, 256)).collect();
        pos += 256 * n2;
        let packed = u32_at(chunk, pos)? as usize;
        let (nmat, npal) = (packed & 0xFFFF, packed >> 16);
        let mats: Vec<String> = (0..nmat).map(|i| cstr(chunk, pos + 4 + 256 * i, 256)).collect();
        let pals: Vec<String> = (0..npal).map(|i| cstr(chunk, pos + 4 + 256 * (nmat + i), 256)).collect();
        pos += 4 + 256 * (nmat + npal);
        let version = u32_at(chunk, pos)?;
        let data_size = u32_at(chunk, pos + 28)? as usize;
        pos += 0x20;
        let end = (pos + data_size).min(chunk.len());
        while pos < end {
            let ntype = u32_at(chunk, pos)?;
            let size = node_size(chunk, pos, ntype, version)?;
            let Some(size) = size.filter(|s| pos + s <= end) else {
                notes.push(format!("render node chunk: node type {:#x} / header version {} not decoded; {} bytes skipped", ntype, version, end - pos));
                break;
            };
            if ntype == NODE_DECAL {
                let q = [f64_at(chunk, pos + 0x40)?, f64_at(chunk, pos + 0x48)?, f64_at(chunk, pos + 0x50)?, f64_at(chunk, pos + 0x58)?];
                let p3 = [f64_at(chunk, pos + 0x60)?, f64_at(chunk, pos + 0x68)?, f64_at(chunk, pos + 0x70)?];
                let s = f64_at(chunk, pos + 0x78)?;
                let mi = u16_at(chunk, pos + 0x80)? as usize;
                if mi < mats.len() && s > 0.0 {
                    ents.push(Entity {
                        name: "decal".into(),
                        cls: Some("Decal".into()),
                        matrix: quat_scale_pos_matrix([q[3], q[0], q[1], q[2]], [s, s, s], p3),
                        decal: Some(mats[mi].replace('\\', "/")),
                        vdr: Some(u8_at(chunk, pos + 0x3E)?),
                        ..Default::default()
                    });
                }
            }
            if ntype == NODE_BRUSH {
                let gi = u16_at(chunk, pos + 0x3C)?;
                if gi == NO_INDEX {
                } else if gi as usize >= geoms.len() {
                    notes.push(format!("render node geometry index {} out of range ({} paths)", gi, geoms.len()));
                } else {
                    let (mo, po) = if version >= 12 { (0xAA, 0xAC) } else { (0xA4, 0xA6) };
                    let mi = if pos + po + 2 <= end { i16_at(chunk, pos + mo)? as i64 } else { -1 };
                    let pi = if pos + po + 2 <= end { u16_at(chunk, pos + po)? as usize } else { 0xFFFF };
                    let mut m = [0.0; 12];
                    for (k, v) in m.iter_mut().enumerate() {
                        *v = f64_at(chunk, pos + 0x40 + 8 * k)?;
                    }
                    ents.push(Entity {
                        name: "rendernode".into(),
                        matrix: m,
                        geometry: Some(geoms[gi as usize].replace('\\', "/").to_lowercase()),
                        mtl: if mi >= 0 && (mi as usize) < mats.len() && !mats[mi as usize].is_empty() { Some(mats[mi as usize].replace('\\', "/")) } else { None },
                        pal: if pi < pals.len() && !pals[pi].is_empty() { Some(pals[pi].replace('\\', "/")) } else { None },
                        vdr: Some(u8_at(chunk, pos + 0x3E)?),
                        ..Default::default()
                    });
                }
            }
            pos += size;
        }
        Ok(())
    })();
    if let Err(e) = r {
        notes.push(format!("render node chunk truncated: {}", e));
    }
    (ents, notes)
}

// ------------------------------------------------------------------------------------------------------------------- containers

/// Container path in one form: lower case, forward slashes, no leading 'data/'.
pub fn container_key(path: &str) -> String {
    let p = path.replace('\\', "/").to_lowercase();
    let p = p.trim_start_matches('/');
    p.strip_prefix("data/").unwrap_or(p).to_string()
}

fn parse_meta_xml(data: &[u8]) -> Option<Element> {
    if cryxml::is_cryxmlb(data) {
        cryxml::parse(data).ok()
    } else {
        plainxml::parse(data).ok()
    }
}

/// Nested containers declared in the container's metadata XML: `<ChildObjectContainers><Child external="1" name="Data/....socpak"
/// guid="..." pos="x,y,z" rot="w,x,y,z"/>`.
///
/// The engine recurses through this list.  Rotation: a
/// quaternion in w,x,y,z order, like the entity attribute Rotate.  EMPIRICAL (all live containers): 19,668 children with a non-trivial
/// rotation also exist as an ObjectContainer entity with the same GUID, and 19,637 of them carry exactly the entity's Rotate string.  There
/// is no scale attribute.  Children without a path or position (location markers, internal groups with external="0") are skipped.  The
/// metadata is plain XML or CryXmlB; `*_editor.xml` copies are ignored.
pub fn read_child_containers(z: &SocZip) -> Vec<Entity> {
    let mut out = vec![];
    for name in z.namelist() {
        let low = name.to_lowercase();
        if !low.ends_with(".xml") || low.ends_with("_editor.xml") {
            continue;
        }
        let Ok(data) = z.read(name) else { continue };
        let Some(root) = parse_meta_xml(&data) else { continue };
        let Some(kids) = root.find("ChildObjectContainers") else { continue };
        for c in &kids.children {
            let path = c.get("name").unwrap_or("").replace('\\', "/").to_lowercase();
            if c.get("external") != Some("1") || !path.ends_with(".socpak") || c.get("pos").is_none() {
                continue;
            }
            let q = floats_or(c.get("rot"), [1.0, 0.0, 0.0, 0.0]);
            let pos = floats_or(c.get("pos"), [0.0; 3]);
            out.push(Entity {
                name: get_t(c, "entityName").map(str::to_owned).unwrap_or_else(|| path.rsplit('/').next().unwrap().to_string()),
                cls: c.get("class").map(str::to_owned),
                matrix: quat_scale_pos_matrix(q, [1.0; 3], pos),
                container: Some(path),
                guid: c.get("guid").map(str::to_owned),
                ..Default::default()
            });
        }
    }
    out
}

/// Rounded position key of the duplicate test (rounded to 2 decimals; -0.0 and 0.0 are one key).
fn round2(v: f64) -> u64 {
    (format!("{:.2}", v).parse::<f64>().unwrap_or(v) + 0.0).to_bits()
}

/// The .soc chunk files inside a socpak: `[{name, size, chunks: [[type, version, size]], statics, nodes}]`, where statics = entities of the
/// embedded `<Entities>` document and nodes = placed render-node geometry.
pub fn list_socs(data: &[u8]) -> Result<Vec<Value>> {
    let z = SocZip::open(data)?;
    let mut out = vec![];
    let names: Vec<&str> = z.namelist().collect();
    for name in names.iter().filter(|n| n.ends_with(".soc")) {
        let soc = z.read(name)?;
        let chunks = soc_chunks(&soc)?;
        let nodes: usize = chunks.iter().filter(|c| c.ctype == RENDER_NODE_CHUNK && c.version == 1).map(|c| read_render_nodes(c.data).0.len()).sum();
        out.push(json!({"name": name, "size": soc.len(),
            "chunks": chunks.iter().map(|c| json!([c.ctype, c.version, c.data.len()])).collect::<Vec<_>>(),
            "statics": read_soc_static_entities(&soc).0.len(), "nodes": nodes}));
    }
    Ok(out)
}

/// Parse a socpak payload.  Returns (entities, errors); `notes` collects known limits (render nodes that are not decoded).
/// `only_soc`: name of one .soc inside the zip; then only that file's static entities and render nodes are returned (no .entxml entities,
/// hence no nested containers).
pub fn read_entities(data: &[u8], notes: Option<&mut Vec<String>>, only_soc: Option<&str>) -> Result<(Vec<Entity>, Vec<String>)> {
    let z = SocZip::open(data)?;
    let (mut ents, mut errs) = (vec![], vec![]);
    let mut notes = notes;
    let names: Vec<&str> = z.namelist().collect();
    if only_soc.is_none() {
        for name in names.iter().filter(|n| n.ends_with(".entxml")) {
            let r = z.read(name).and_then(|d| cryxml::parse(&d));
            match r {
                Ok(root) => ents.push(entity_from_root(&root, name)),
                Err(ex) => errs.push(format!("{}: {}", name, ex)),
            }
        }
    }
    for name in names.iter().filter(|n| n.ends_with(".soc") && only_soc.map_or(true, |o| o == **n)) {
        let soc = z.read(name)?;
        let (st, er) = read_soc_static_entities(&soc);
        ents.extend(st);
        errs.extend(er);
        for ch in soc_chunks(&soc)? {
            if ch.ctype == RENDER_NODE_CHUNK && ch.version == 1 {
                let (rn, rn_notes) = read_render_nodes(ch.data);
                ents.extend(rn);
                let pal = render_node_palettes(ch.data);
                if !pal.is_empty() {
                    // not an entity: the palette names of this container (collect_instances counts them)
                    ents.push(Entity { name: "palette-table".into(), cls: Some("PaletteTable".into()), matrix: IDENT, palettes: pal, ..Default::default() });
                }
                if let Some(n) = notes.as_deref_mut() {
                    n.extend(rn_notes);
                }
            }
        }
    }
    if only_soc.is_none() {
        // a child declared both as an ObjectContainer entity and in the metadata is ONE container: same GUID (EMPIRICAL: 36,802 of the
        // 36,860 metadata children with a rotation in the live archive have an entity with their GUID), else same path and position.  The
        // paths differ in form (the entity's objectContainer= has no 'Data/', the metadata name has): those containers were drawn twice
        // (7,218 duplicate instances in asd_layout_facility_001) before this test.
        merge_declared_children(&mut ents, read_child_containers(&z));
    }
    Ok((ents, errs))
}

/// Merge the children declared in the metadata XML into the entity list (see `read_entities` for the duplicate rule).
///
/// Placement rule (2026-11-03): where the SAME container (same GUID) is both an ObjectContainer entity and a declared child, the declared
/// `pos/rot` is the placement in this container's own frame, NOT the entity's `Pos/Rotate`.  The entity transform is in the authoring level's
/// world: entity = Pivot * declared, a rigid transform that is constant per container.  EMPIRICAL on the open dungeons: P2L4_ContestedZone
/// has 481 ObjectContainer entities and every one is exactly (-92, 802, 0) from its declared child (same rotation); the container's own
/// frame (declared minBounds/maxBounds -199..201 x -49..451, goto points, interior-map sections, 817 loot points, its own brush) is the
/// declared one, and with it all 817 loot points land on the module geometry (with the entity positions, 332).  Other pivots seen: a pure
/// translation (rs_cargo_001 (0,167,0), rs_refinery_pyro (-271,0,0), rs_habs_lobby (-287,18,84)) and a 180 degree turn about z
/// (rs_worker_p6-l1).  CONFIRMED against the engine (static): the Child pos/rot positions the container node.  INFERRED: that the entity transform is never used for the placement of these children.
/// The entity's `Scale` (the declaration has none) is kept.
pub fn merge_declared_children(ents: &mut Vec<Entity>, kids: Vec<Entity>) {
    let key = |e: &Entity| (container_key(e.container.as_deref().unwrap_or("")), round2(e.matrix[3]), round2(e.matrix[7]), round2(e.matrix[11]));
    let by_guid: std::collections::HashMap<String, usize> = ents
        .iter()
        .enumerate()
        .filter(|(_, e)| e.container.is_some())
        .filter_map(|(i, e)| e.guid.as_deref().filter(|g| !g.is_empty()).map(|g| (g.to_lowercase(), i)))
        .collect();
    let have: HashSet<_> = ents.iter().filter(|e| e.container.is_some()).map(key).collect();
    for ch in kids {
        let g = ch.guid.as_deref().unwrap_or("").to_lowercase();
        if let Some(&i) = by_guid.get(&g) {
            let m = &mut ents[i].matrix;
            // column lengths of the entity matrix = its scale; the declared matrix is unscaled
            let sc = [0, 1, 2].map(|c| (m[c] * m[c] + m[4 + c] * m[4 + c] + m[8 + c] * m[8 + c]).sqrt());
            for r in 0..3 {
                for c in 0..3 {
                    m[4 * r + c] = ch.matrix[4 * r + c] * sc[c];
                }
                m[4 * r + 3] = ch.matrix[4 * r + 3];
            }
        } else if !have.contains(&key(&ch)) {
            ents.push(ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_quaternion() {
        let m = quat_scale_pos_matrix([1.0, 0.0, 0.0, 0.0], [1.0; 3], [1.0, 2.0, 3.0]);
        assert_eq!(m, [1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0]);
        assert_eq!(compose(&IDENT, &m), m);
    }

    #[test]
    fn compose_translates() {
        let a = quat_scale_pos_matrix([1.0, 0.0, 0.0, 0.0], [2.0, 2.0, 2.0], [10.0, 0.0, 0.0]);
        let b = quat_scale_pos_matrix([1.0, 0.0, 0.0, 0.0], [1.0; 3], [1.0, 0.0, 0.0]);
        assert_eq!(compose(&a, &b)[3], 12.0);
    }

    #[test]
    fn floats_need_exact_count() {
        assert_eq!(floats::<3>(Some("1, 2,3")), Some([1.0, 2.0, 3.0]));
        assert_eq!(floats::<3>(Some("1,2")), None);
        assert_eq!(floats::<3>(None), None);
    }

    #[test]
    fn bare_reference() {
        let e = Element { tag: "Entity".into(), attrs: vec![("EntityCryGUID".into(), "x".into())], ..Default::default() };
        assert!(is_bare_reference(&e));
    }

    #[test]
    fn declared_child_wins_over_entity_transform() {
        let ent = |guid: &str, m: [f64; 12]| Entity { name: "e".into(), container: Some("objectcontainers/a.socpak".into()), guid: Some(guid.into()), matrix: m, ..Default::default() };
        // entity = declared + (-92, 802, 0) (the P2L4_ContestedZone pivot), scale 2 on the entity
        let e = ent("AA", quat_scale_pos_matrix([1.0, 0.0, 0.0, 0.0], [2.0; 3], [14.0, 917.0, 0.0]));
        let declared = ent("aa", quat_scale_pos_matrix([1.0, 0.0, 0.0, 0.0], [1.0; 3], [106.0, 115.0, 0.0]));
        // a declared child with no entity is appended, one at an entity's path + position is dropped
        let extra = Entity { guid: Some("bb".into()), ..ent("bb", quat_scale_pos_matrix([1.0, 0.0, 0.0, 0.0], [1.0; 3], [1.0, 2.0, 3.0])) };
        let mut v = vec![e];
        merge_declared_children(&mut v, vec![declared, extra]);
        assert_eq!(v.len(), 2);
        assert_eq!([v[0].matrix[3], v[0].matrix[7], v[0].matrix[11]], [106.0, 115.0, 0.0]);
        assert_eq!(v[0].matrix[0], 2.0, "entity scale kept");
        assert_eq!(v[1].matrix[3], 1.0);
    }

    #[test]
    fn container_keys() {
        assert_eq!(container_key("Data\\ObjectContainers\\A.socpak"), "objectcontainers/a.socpak");
        assert_eq!(container_key("/objectcontainers/a.socpak"), "objectcontainers/a.socpak");
    }

    #[test]
    fn empty_render_chunk_is_truncated_note() {
        let (e, notes) = read_render_nodes(&[0, 0]);
        assert!(e.is_empty());
        assert!(notes[0].starts_with("render node chunk truncated"));
    }
}
