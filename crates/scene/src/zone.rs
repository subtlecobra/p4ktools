//! Zone compiler: a whole `.socpak` flattened into the data the zone viewer streams (the interaction catalog, the
//! elevator cars and the MFD / panel screen roles are in the `inter` submodule).
//!
//! A [`Zone`] is compiled once (instances + one bounding sphere per distinct geometry; the per-geometry sphere work runs on rayon) and
//! then serves, on demand and byte-compatibly with the viewer's `/zonedata/<file>?k=...` feed:
//!
//! * `info()`          JSON: geometries (name, sphere, LOD ids), bbox, notes, decal table, palettes ...;
//! * `instances()`     binary: u32 N, u32 gid[N], f32 mat[N][12] (row-major 3x4), u8 palette slot[N]; sorted by gid;
//! * `geometry(gid)`   binary mesh of one geometry (render parts only, merged by material), with the stored tangent frame;
//! * `collision(gid)`  binary collision / proxy meshes of a geometry (built together with its render mesh);
//! * `texture(...)`    raw BC mips or a PNG (`texserve`);
//! * `decals()`, `volumes()`, `lights()`, `palette(path)`, `palette_names()`, `hue_lut()`.
//!
//! [`ZoneJob`] is the background-thread wrapper with the progress status of `k=status`; [`ZoneJob::serve`] answers one feed request, so a
//! server only has to parse the query string.
//!
//! LODs: the archive stores `<stem>_lod1.cgf` ... next to `<stem>.cgf` (EMPIRICAL: 72 of the first 300 geometries of
//! asd_layout_facility_001 have a _lod1 sibling).  They are registered as extra geometries with the same sphere and listed in
//! `geoms[g]["lods"]`; the viewer chooses by distance.  Which LOD the engine would pick at which distance is NOT known here.
//!
//! Bounding spheres come from the .cgf node boxes (node-local, moved by the node's matrix) united with the mesh companion's header box
//! (see `compile_sphere`); the mesh file is not otherwise read at compile time.
//!
//! Doors / seats / turrets / panels / ladders (`k=inter`), the elevator cars (`k=elev`), the moving-part split of door and seat models
//! (`geoms[].sub`, per-instance `link`s and riders) and the MFD / panel screen roles of the geo mats are compiled when the zone has
//! item records and `ZoneSpec::no_catalog` is false (the server's behaviour).  A zone of a ship is given its `parts` / `hull` / `extras` /
//! `livery` (the caller computes them from the DataCore, see `ships::Ships::ship_zone_spec`), see [`ZoneSpec`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use p4k_formats::adb;
use p4k_formats::ivo::{self, Node};
use p4k_formats::socpak::{compose, IDENT};
use p4k::P4k;
use rayon::prelude::*;
use serde_json::{json, Map, Value};

use crate::codec::{Reader, Writer};
use crate::builder::{self, build_scene_named, companion_exts, lod_base, material_path, normalise_material_name, resolve_material_path, GeoSource, Options};
use crate::elevator;
use crate::interact::{Catalog, Desc, Dg};
use crate::itemmaterials::ItemMaterials;
use crate::itemwear;
use crate::materials::{rnd, MatRegistry};
use crate::mfd;
use crate::overlay::Overlay;
use crate::panelstyle::PanelStyle;
use crate::socpak_scene::{collect_instances_cat, missing_geometry_note, no_geometry_note, summarise, Instance, Stats, Tag};

mod inter;
use inter::{compile_elevators, compile_interactables, door_key, Car, Ctx, Leaf, Tagged};
use crate::texserve::{self, TexRef, TextureHost};

/// per geometry; zones are drawn from the GPU.  EMPIRICAL: the ANVL Lightning F8 hull has 354,651 render triangles, and the former
/// 200,000 cut dropped the nodes at the end of the file (both wings)
pub const ZONE_RENDER_TRIS: usize = 4_000_000;
pub const LOD_MAX: usize = 4;
/// per-instance palette slot byte: 0 = none, 1..31 = info.palslots
pub const PAL_SLOTS: usize = 32;
/// serve the stored tangent frame in geo blobs (the viewer adopts it later)
pub const ZONE_FRAMES: bool = true;

type Mat = [f64; 12];
pub type Sphere = ([f64; 3], f64);

// ---------------------------------------------------------------------------------------------------------------------------
// helpers

/// Material path in a comparable form: lower case, no leading slash / 'data/', no extension.
pub fn mkey(path: &str) -> String {
    let p = path.replace('\\', "/").to_lowercase();
    let p = p.trim_start_matches('/');
    let p = p.strip_prefix("data/").unwrap_or(p);
    p.strip_suffix(".mtl").unwrap_or(p).to_string()
}

fn ok_box(b: &[f64]) -> bool {
    b.iter().all(|x| x.abs() < 1e6)
}

/// (centre, radius) of the cgf's nodes in object space, or None when no node carries a usable box.
pub fn sphere_of(nodes: &[Node]) -> Option<Sphere> {
    let (mut lo, mut hi) = ([1e30f64; 3], [-1e30f64; 3]);
    for n in nodes {
        let b = n.bbox;
        if !ok_box(&b) || b[0] > b[3] || b[1] > b[4] || b[2] > b[5] {
            continue;
        }
        let gm = ivo::geometry_matrix(Some(n));
        for x in [b[0], b[3]] {
            for y in [b[1], b[4]] {
                for z in [b[2], b[5]] {
                    let w = ivo::xform(&gm, [x, y, z]);
                    for k in 0..3 {
                        lo[k] = lo[k].min(w[k]);
                        hi[k] = hi[k].max(w[k]);
                    }
                }
            }
        }
    }
    if lo[0] > hi[0] {
        return None;
    }
    let c = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
    let d: f64 = (0..3).map(|k| (hi[k] - lo[k]).powi(2)).sum::<f64>().sqrt() / 2.0;
    Some((c, d.max(0.05)))
}

/// (centre, radius) from the mesh companion's header box, or None.  EMPIRICAL: many cgf files have no nodes at all (5,973 of 14,428
/// geometries of asd_layout_facility_001 incl. LODs) while the .cgfm header carries the bounds in object space for single-object
/// meshes; for multi-node files this ignores the node matrices (approximate).
pub fn mesh_sphere(src: &GeoSource, name: &str) -> Result<Option<Sphere>, String> {
    let stem = &name[..name.rfind('.').unwrap_or(name.len())];
    for ext in companion_exts(name) {
        // the model's own mesh file first (a .cga and a .cgf of one stem have different meshes)
        let cname = format!("{}{}", stem, ext);
        let Some(data) = src.read(&cname) else { continue };
        let data = data?;
        let meshes = ivo::parse_cgfm(&data).map_err(|e| e.to_string())?;
        let Some(m) = meshes.first() else { return Ok(None) };
        let (lo, hi) = (m.bbox_min, m.bbox_max);
        let all: Vec<f64> = lo.iter().chain(hi.iter()).copied().collect();
        if ok_box(&all) && (0..3).all(|k| lo[k] <= hi[k]) {
            let c = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
            let d = (0..3).map(|k| (hi[k] - lo[k]).powi(2)).sum::<f64>().sqrt() / 2.0;
            return Ok(Some((c, d.max(0.05))));
        }
    }
    Ok(None)
}

/// Smallest sphere enclosing two (centre, radius) spheres; None entries are ignored.
pub fn union_sphere(a: Option<Sphere>, b: Option<Sphere>) -> Option<Sphere> {
    let (Some((ca, ra)), Some((cb, rb))) = (a, b) else { return a.or(b) };
    let d = (0..3).map(|k| (ca[k] - cb[k]).powi(2)).sum::<f64>().sqrt();
    if d + rb <= ra {
        return a;
    }
    if d + ra <= rb {
        return b;
    }
    let r = (d + ra + rb) / 2.0;
    let t = (r - ra) / d;
    Some(([ca[0] + (cb[0] - ca[0]) * t, ca[1] + (cb[1] - ca[1]) * t, ca[2] + (cb[2] - ca[2]) * t], r))
}

/// Node-box sphere united with the mesh header sphere.  EMPIRICAL (Idris bridge: 107 of 195 geometries): a node box can be
/// degenerate or off (radius 0.1 for a 23 m window frame, centre 5 m away), which culled the piece whenever that point left the
/// screen; so the node box and the mesh header box are united.  Err = the file could not be read or parsed.
pub fn compile_sphere(src: &GeoSource, name: &str) -> Result<Option<Sphere>, String> {
    let data = src.read(name).ok_or_else(|| format!("KeyError: {}", name))??;
    let cgf = ivo::parse_cgf(&data, false).map_err(|e| e.to_string())?;
    Ok(union_sphere(sphere_of(&cgf.nodes), mesh_sphere(src, name)?))
}

/// 8 bytes of the zone geo tangent frame for normal `n` and tangent `t` (x, y, z, handedness): int8 snorm normal x y z, handedness
/// (+-127; 0 = no tangent stream), int8 snorm tangent x y z, 0 (bitangent = handed * cross(normal, tangent)).
pub fn pack_frame(n: [f64; 3], t: [f64; 4]) -> [u8; 8] {
    let q = |c: f64| (c * 127.0).round_ties_even().clamp(-127.0, 127.0) as i8 as u8;
    [q(n[0]), q(n[1]), q(n[2]), ((t[3] as i64 * 127).clamp(-127, 127) as i8) as u8, q(t[0]), q(t[1]), q(t[2]), 0]
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(b' ');
    }
}

fn with_len(head: Vec<u8>, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + head.len() + body.len());
    out.extend((head.len() as u32).to_le_bytes());
    out.extend(head);
    out.extend_from_slice(body);
    out
}

fn r4(v: f64) -> Value {
    json!(rnd(v, 4))
}

fn r5(v: f64) -> Value {
    json!(rnd(v, 5))
}

fn arr_f(v: &Value) -> Vec<f64> {
    v.as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect()).unwrap_or_default()
}

fn m12(v: &Value) -> Mat {
    let a = arr_f(v);
    let mut m = IDENT;
    for (i, x) in a.iter().take(12).enumerate() {
        m[i] = *x;
    }
    m
}

fn v3(v: &Value) -> [f64; 3] {
    let a = arr_f(v);
    [a.first().copied().unwrap_or(0.0), a.get(1).copied().unwrap_or(0.0), a.get(2).copied().unwrap_or(0.0)]
}

/// `round(v, 5)` of every entry of a matrix.
fn rm5(m: &Mat) -> Value {
    Value::Array(m.iter().map(|v| r5(*v)).collect())
}

// ---------------------------------------------------------------------------------------------------------------------------
// inputs and outputs

/// A livery: the palette is given to the hull, the loadout items and every container object that has none of its own;
/// its material replaces the hull model's.
#[derive(Debug, Clone, Default)]
pub struct Livery {
    pub id: Option<String>,
    pub palette: Option<String>,
    pub material: Option<String>,
}

/// A model drawn in addition to the containers (a ship's loadout item).
#[derive(Debug, Clone, Default)]
pub struct Extra {
    pub path: String,
    pub matrix: Mat,
    pub cls: Option<String>,
    pub port: Option<String>,
    pub uid: Option<i64>,
    pub parent: Option<i64>,
    pub helper: Option<String>,
    /// the item's own palette (`paints::class_palette`); the ship's livery palette applies when None
    pub pal: Option<String>,
}

/// What to compile: one container (`entry`), or a ship (`parts` placed by matrices, `hull` model at the origin, `extras`).
#[derive(Debug, Clone, Default)]
pub struct ZoneSpec {
    /// archive name of the `.socpak` (a plain zone)
    pub entry: Option<String>,
    pub max_instances: Option<usize>,
    pub only_soc: Option<String>,
    /// (container entry name, 3x4 matrix): a ship is several containers placed by offsets
    pub parts: Option<Vec<(String, Mat)>>,
    /// shown as the asset name
    pub title: Option<String>,
    /// archive path of the exterior model (drawn at the origin)
    pub hull: Option<String>,
    pub extras: Vec<Extra>,
    pub livery: Option<Livery>,
    /// lines the caller wants in the info notes
    pub extra_notes: Vec<String>,
    /// vehicle commands of a ship's Controller_* entities (ships.rs `ship_commands`): added to the commands of every seat
    pub ship_cmds: Vec<Value>,
    /// switch the interaction catalog, elevator cars and screen roles off (`k=inter` / `k=elev` then stay empty, nothing is split):
    /// the zone of the parity tests; the server never sets it
    pub no_catalog: bool,
}

impl ZoneSpec {
    /// From a `spec.json` (usable as a command line / test fixture):
    /// `{"kind": "container", "entry": ...}` or `{"kind": "ship", "parts": [[entry, [12 floats]]], "hull", "title", "extras": [...],
    /// "livery", "extra_notes"}`.
    pub fn from_json(s: &Value) -> ZoneSpec {
        let st = |v: &Value| v.as_str().map(str::to_string);
        if s["kind"] == "container" {
            return ZoneSpec { entry: st(&s["entry"]), ..Default::default() };
        }
        ZoneSpec {
            parts: Some(s["parts"].as_array().map_or(vec![], |a| a.iter().map(|p| (p[0].as_str().unwrap_or("").to_string(), m12(&p[1]))).collect())),
            title: st(&s["title"]),
            hull: st(&s["hull"]),
            extras: s["extras"]
                .as_array()
                .map_or(vec![], |a| {
                    a.iter()
                        .map(|x| Extra {
                            path: x["path"].as_str().unwrap_or("").to_string(),
                            matrix: m12(&x["matrix"]),
                            cls: st(&x["cls"]),
                            port: st(&x["port"]),
                            uid: x["uid"].as_i64(),
                            parent: x["parent"].as_i64(),
                            helper: st(&x["helper"]),
                            pal: st(&x["pal"]),
                        })
                        .collect()
                }),
            livery: s["livery"].as_object().map(|l| Livery { id: st(&l["id"]), palette: st(&l["palette"]), material: st(&l["material"]) }),
            extra_notes: s["extra_notes"].as_array().map_or(vec![], |a| a.iter().filter_map(|n| n.as_str().map(str::to_string)).collect()),
            ship_cmds: s["ship_cmds"].as_array().cloned().unwrap_or_default(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Geom {
    pub name: String,
    pub c: [f64; 3],
    pub r: f64,
    pub lods: Vec<usize>,
    pub n: usize,
    /// the base geometry of a LOD file (collision comes from the base); -1 for a base geometry
    pub p: i64,
    pub mtl: Option<String>,
    /// authored default wear / dirt (the viewer's 'auto' wear mode)
    pub wear: Option<(f64, f64)>,
    /// only these nodes (`inside` true) / all but these (false) of the model: a door's, seat's or turret's moving parts
    pub sub: Option<SubSpec>,
}

/// The `sub` of a geometry entry: `(inside, node indices)`, JSON `["in" | "out", [nodes]]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubSpec {
    pub inside: bool,
    pub nodes: Vec<usize>,
}

impl SubSpec {
    /// Python `repr` of `("in", (n, ...))` (the compile sorts the geometries by it).
    fn repr(s: &Option<SubSpec>) -> String {
        match s {
            None => "None".to_string(),
            Some(s) => {
                let t = match s.nodes.len() {
                    1 => format!("({},)", s.nodes[0]),
                    _ => format!("({})", s.nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")),
                };
                format!("('{}', {})", if s.inside { "in" } else { "out" }, t)
            }
        }
    }
}

impl Geom {
    pub fn to_json(&self) -> Value {
        let mut g = Map::new();
        g.insert("name".into(), json!(self.name));
        g.insert("c".into(), Value::Array(self.c.iter().map(|v| r4(*v)).collect()));
        g.insert("r".into(), r4(self.r));
        g.insert("lods".into(), json!(self.lods));
        g.insert("n".into(), json!(self.n));
        g.insert("p".into(), json!(self.p));
        if let Some(m) = &self.mtl {
            g.insert("mtl".into(), json!(m));
        }
        if let Some((w, d)) = self.wear {
            g.insert("w".into(), json!(w));
            g.insert("dt".into(), json!(d));
        }
        if let Some(s) = &self.sub {
            g.insert("sub".into(), json!([if s.inside { "in" } else { "out" }, s.nodes]));
        }
        Value::Object(g)
    }
}

impl Geom {
    /// Inverse of [`Geom::to_json`] (the compile snapshot stores geometries that way); `None` when a field is missing.
    pub fn from_json(v: &Value) -> Option<Geom> {
        let c = v.get("c")?.as_array()?;
        let sub = match v.get("sub") {
            Some(s) => Some(SubSpec {
                inside: s.get(0)?.as_str()? == "in",
                nodes: s.get(1)?.as_array()?.iter().map(|n| n.as_u64().map(|n| n as usize)).collect::<Option<Vec<_>>>()?,
            }),
            None => None,
        };
        Some(Geom {
            name: v.get("name")?.as_str()?.to_string(),
            c: [c.first()?.as_f64()?, c.get(1)?.as_f64()?, c.get(2)?.as_f64()?],
            r: v.get("r")?.as_f64()?,
            lods: v.get("lods")?.as_array()?.iter().map(|n| n.as_u64().map(|n| n as usize)).collect::<Option<Vec<_>>>()?,
            n: v.get("n")?.as_u64()? as usize,
            p: v.get("p")?.as_i64()?,
            mtl: v.get("mtl").and_then(Value::as_str).map(str::to_string),
            wear: match (v.get("w"), v.get("dt")) {
                (Some(w), Some(d)) => Some((w.as_f64()?, d.as_f64()?)),
                _ => None,
            },
            sub,
        })
    }
}

/// A built geometry: the mesh blob and the collision blob (None when it has none).
pub struct GeoBlobs {
    pub mesh: Vec<u8>,
    pub col: Option<Vec<u8>>,
}

/// One flattened record before grouping.
struct Rec {
    name: String,
    m: Mat,
    mtl: Option<String>,
    pal: Option<String>,
    /// the interaction tag (door / seat / turret / panel), only with a catalog
    tag: Option<Tag>,
    /// the elevator car this record is a part of
    car: Option<usize>,
    /// a loadout item (hangs on a helper of its parent item) or an entity hung on a helper of another entity of its container
    ride: Option<RideRec>,
}

/// Identity of an entity / loadout item for the `ride` links: the loadout uid, or ("e", container instance, entity GUID).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    U(i64),
    E(usize, String),
}

/// A ride record: `{"uid", "parent", "helper"[, "att": true]}`.
#[derive(Debug, Clone)]
struct RideRec {
    uid: Key,
    parent: Option<Key>,
    helper: Option<String>,
    /// hung on a helper of another entity of its container: placed after its parent
    att: bool,
}

/// Where the instance index of a split record is recorded (the target record and the instance index).
#[derive(Debug, Clone, Copy)]
enum LinkTo {
    /// `tagged[i].leaves`
    Leaves(usize),
    /// `tagged[i].riders`
    Riders(usize),
    /// `cars[i].leaves`
    Car(usize),
}

#[derive(Debug, Clone, Copy)]
struct Link {
    to: LinkTo,
    gi: Option<usize>,
}

type GKey = (String, Option<String>, Option<SubSpec>);

/// Add the car container of every elevator (`elevator`) to `out` at its starting stop; each of its records carries the car index
/// so the compile can index its instances.  The car's decals, lights and volumes are dropped (they come from static feeds and would
/// hang in the shaft when the car leaves); its AreaBox gives the rider volume.
fn place_cars(p4k: &P4k, out: &mut Vec<Rec>, stats: &mut Stats, isrc: Option<&dyn crate::items::ItemMaterialSource>, cat: Option<&Catalog>) -> Vec<Car> {
    let mut cars: Vec<Car> = vec![];
    let plans = elevator::plan(&stats.transit, &stats.transit_ents, &stats.transit_doors, Some(p4k), None);
    for pl in plans {
        let Some(cab) = p4k.find_path(&crate::socpak_scene::data_path(&pl.cab.replace('\\', "/").to_lowercase()), ".socpak") else {
            stats.notes.push(format!("elevator car container {} is not in the archive", pl.cab));
            continue;
        };
        let s0 = pl.stops[pl.at].m;
        let before = (stats.decals.len(), stats.lights.len(), stats.volumes.len());
        let mut recs: Vec<Instance> = vec![];
        collect_instances_cat(p4k, cab, &s0, 0, &mut recs, stats, &HashSet::new(), None, isrc, cat);
        let vols: Vec<(Value, Mat)> = stats.volumes[before.2.min(stats.volumes.len())..].to_vec();
        stats.decals.truncate(before.0);
        stats.lights.truncate(before.1);
        stats.volumes.truncate(before.2);
        let id = cars.len();
        let panel = elevator::car_geometry(&stats.cached_entities(&cab.name, None).map(|e| e.to_vec()).unwrap_or_default());
        let n = recs.len();
        cars.push(Car { box_: elevator::car_box(&vols, &s0), plan: pl, leaves: vec![], id, panel, n });
        for r in recs {
            out.push(Rec { name: r.geometry, m: r.matrix, mtl: r.mtl, pal: r.pal, tag: r.tag, car: Some(id), ride: None });
        }
    }
    cars
}

/// `(wear, dirt)` floors rounded (Python `round` semantics), None when both are zero.
fn floors_pair(fl: [f64; 4]) -> Option<(f64, f64)> {
    (fl[0] > 0.0 || fl[1] > 0.0).then(|| (rnd(fl[0], 3), rnd(fl[1], 3)))
}

fn gidx_get<'a>(p4k: &'a P4k, path: &str) -> Option<&'a p4k::Entry> {
    let key = path.replace('\\', "/").to_lowercase();
    let key = key.trim_start_matches('/').to_string();
    for k in [key.clone(), format!("data/{}", key)] {
        for ext in [".cgf", ".cga"] {
            if let Some(&i) = p4k.lower_index(ext).map.get(&k) {
                return Some(&p4k.entries[i]);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------------------------------------
// the zone

const SNAP_MAGIC: &[u8; 4] = b"ZSN1";

pub struct Zone<'a> {
    p4k: &'a P4k,
    items: Option<&'a ItemMaterials>,
    spec: ZoneSpec,
    overlay: Overlay,
    registry: Mutex<MatRegistry<'a>>,
    texhost: TextureHost<'a>,
    geoms: RwLock<Vec<Geom>>,
    gid_of: Mutex<HashMap<GKey, usize>>,
    own: Mutex<HashMap<String, Option<String>>>,
    info: Vec<u8>,
    inst: Vec<u8>,
    decals: Vec<u8>,
    volumes: Vec<u8>,
    lights: Vec<u8>,
    inter: Vec<u8>,
    elev: Vec<u8>,
    /// geometry key (`mfd::geo_key`) -> does the panel model have a display (set by the compile, `inter::panel_display`)
    panel_geos: HashMap<String, bool>,
    /// `mfd::dashboard_geometries`, built when the first geometry is (0.5 s)
    dash: OnceLock<Map<String, Value>>,
    pal_slots: Vec<String>,
    livery_gids: Vec<usize>,
    geo_cache: Mutex<HashMap<usize, Arc<GeoBlobs>>>,
    liv_cache: Mutex<HashMap<(usize, Option<String>), Arc<Vec<u8>>>>,
    tex_lock: Mutex<()>,
    instance_count: usize,
}

const DRAWN_NOTE: &str = "Drawn: Illum diffuse / normal / specular / gloss maps, the BLENDLAYER second layer, detail maps and parallax occlusion mapping; LayerBlend_V2 and HardSurface base layers, blend map, tints, AO, layer normals, gloss and specular, the hue shift table, tint palettes (per object, base and wear layers), wear layers behind the Wear slider (LayerBlend's wear mask is approximate, HardSurface follows the engine's formula), HardSurface macro AO / normal / gloss / emissive maps, POM and the Primary layer's specular and detail maps; GlassPBR as absorption + fresnel (no refraction, thin film, emissive, cracks, frost); mesh decals (MeshDecal and Illum %DECAL: opacity shaping, vertex colour / data, glow, normal / specular maps, POM, stencil marks from the palette's logo map). Not drawn: dirt and stains (they need per-object state the data does not have), damage maps, the HardSurface Secondary layer, iridescence, MeshDecal wear / tint palette modes";

/// Counts in order of first appearance (insertion order; a stable sort by descending count keeps it for ties).
#[derive(Default)]
struct Counts {
    order: Vec<String>,
    n: HashMap<String, usize>,
}

impl Counts {
    fn add(&mut self, k: &str) {
        let e = self.n.entry(k.to_string()).or_insert(0);
        if *e == 0 {
            self.order.push(k.to_string());
        }
        *e += 1;
    }

    fn sorted_desc(&self) -> Vec<(&str, usize)> {
        let mut v: Vec<(&str, usize)> = self.order.iter().map(|k| (k.as_str(), self.n[k])).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }

    fn to_json(&self) -> Value {
        Value::Object(self.order.iter().map(|k| (k.clone(), json!(self.n[k]))).collect())
    }
}

struct VolOut {
    json: Vec<u8>,
    counts: Value,
    boxes: usize,
    rooms: usize,
    shapes: usize,
    fogs: usize,
    actions: usize,
    navs: usize,
    rconns: usize,
    ledges: usize,
    covers: usize,
    dboxes: usize,
    splines: usize,
    markers: Counts,
}

/// AreaBox boxes (12 floats each: the matrix taking the unit cube [-1, 1]^3 to the world box), Room spheres (the matrix taking the
/// unit sphere to the entity's bounding Radius) and AreaShape prisms (world-space line segments).  See `socpak::volume_info` for what
/// each class stores and what is guessed.
fn compile_volumes(stats: &Stats) -> VolOut {
    let (mut boxes, mut rooms, mut shapes, mut nshapes) = (vec![], vec![], vec![], 0usize);
    let (mut fogs, mut actions, mut navs, mut markers, mut nnavs) = (vec![], vec![], vec![], vec![], 0usize);
    let (mut rconns, mut ledges, mut covers, mut nledges, mut ncovers) = (vec![], vec![], vec![], 0usize, 0usize);
    let (mut dboxes, mut splines, mut nsplines) = (vec![], vec![], 0usize);
    let seg = |out: &mut Vec<Value>, base: &[[f64; 3]], top: &[[f64; 3]]| {
        for i in 0..base.len() {
            let j = (i + 1) % base.len();
            for (a, b) in [(base[i], base[j]), (top[i], top[j]), (base[i], top[i])] {
                for v in a.iter().chain(b.iter()) {
                    out.push(r4(*v));
                }
            }
        }
    };
    let half = |v: &Value, floor: f64| -> [f64; 3] {
        let s = v3(v);
        [(s[0] / 2.0).max(floor), (s[1] / 2.0).max(floor), (s[2] / 2.0).max(floor)]
    };
    let radius_row = |r: f64, m: &Mat| json!([r, 0, 0, rnd(m[3], 5), 0, r, 0, rnd(m[7], 5), 0, 0, r, rnd(m[11], 5)]);
    for (vol, m) in &stats.volumes {
        match vol["kind"].as_str().unwrap_or("") {
            "areabox" => {
                let (lo, hi) = (v3(&vol["box"][0]), v3(&vol["box"][1]));
                let c = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
                let h = [((hi[0] - lo[0]) / 2.0).max(1e-4), ((hi[1] - lo[1]) / 2.0).max(1e-4), ((hi[2] - lo[2]) / 2.0).max(1e-4)];
                let unit = [h[0], 0.0, 0.0, c[0], 0.0, h[1], 0.0, c[1], 0.0, 0.0, h[2], c[2]];
                boxes.push(rm5(&compose(m, &compose(&m12(&vol["local"]), &unit))));
            }
            "room" => {
                let r = vol["radius"].as_f64().unwrap_or(0.0);
                if r > 0.0 {
                    rooms.push(radius_row(r, m));
                }
            }
            k @ ("areashape" | "navarea") => {
                let full = compose(m, &m12(&vol["local"]));
                let h = vol["height"].as_f64().unwrap_or(0.0);
                let pts: Vec<[f64; 3]> = vol["pts"].as_array().map(|a| a.iter().map(v3).collect()).unwrap_or_default();
                let base: Vec<[f64; 3]> = pts.iter().map(|p| ivo::xform(&full, *p)).collect();
                let top: Vec<[f64; 3]> = pts.iter().map(|p| ivo::xform(&full, [p[0], p[1], p[2] + h])).collect();
                if k == "areashape" {
                    nshapes += 1;
                    seg(&mut shapes, &base, &top);
                } else {
                    nnavs += 1;
                    seg(&mut navs, &base, &top);
                }
            }
            "actionarea" => {
                let r = vol["radius"].as_f64().unwrap_or(0.0);
                if r > 0.0 {
                    actions.push(radius_row(r, m));
                }
            }
            "fog" => {
                let h = half(&vol["size"], 1e-3);
                fogs.push(rm5(&compose(m, &[h[0], 0.0, 0.0, 0.0, 0.0, h[1], 0.0, 0.0, 0.0, 0.0, h[2], 0.0])));
            }
            "roomconnector" => {
                let h = half(&vol["size"], 1e-3);
                let o = v3(&vol["offset"]);
                rconns.push(rm5(&compose(m, &[h[0], 0.0, 0.0, o[0], 0.0, h[1], 0.0, o[1], 0.0, 0.0, h[2], o[2]])));
            }
            "ledge" => {
                nledges += 1;
                let h = vol["height"].as_f64().unwrap_or(0.0);
                let pts: Vec<[f64; 3]> = vol["pts"].as_array().map(|a| a.iter().map(v3).collect()).unwrap_or_default();
                let base: Vec<[f64; 3]> = pts.iter().map(|p| ivo::xform(m, *p)).collect();
                let top: Vec<[f64; 3]> = pts.iter().map(|p| ivo::xform(m, [p[0], p[1], p[2] + h])).collect();
                seg(&mut ledges, &base, &top);
            }
            "cover" => {
                ncovers += 1;
                let samples: Vec<Vec<f64>> = vol["samples"].as_array().map(|a| a.iter().map(arr_f).collect()).unwrap_or_default();
                let pts: Vec<([f64; 3], [f64; 3])> = samples
                    .iter()
                    .map(|s| (ivo::xform(m, [s[0], s[1], s[2]]), ivo::xform(m, [s[0], s[1], s[2] + s[3]])))
                    .collect();
                for (i, (lo, hi)) in pts.iter().enumerate() {
                    for v in lo.iter().chain(hi.iter()) {
                        covers.push(r4(*v)); // the sample's height
                    }
                    if i > 0 {
                        for v in pts[i - 1].1.iter().chain(hi.iter()) {
                            covers.push(r4(*v)); // along the tops
                        }
                    }
                }
            }
            "sizedbox" => {
                let h = half(&vol["size"], 1e-3);
                dboxes.push(rm5(&compose(m, &[h[0], 0.0, 0.0, 0.0, 0.0, h[1], 0.0, 0.0, 0.0, 0.0, h[2], 0.0])));
            }
            "spline" => {
                nsplines += 1;
                let pts: Vec<[f64; 3]> = vol["pts"].as_array().map(|a| a.iter().map(|p| ivo::xform(m, v3(p))).collect()).unwrap_or_default();
                for w in pts.windows(2) {
                    for v in w[0].iter().chain(w[1].iter()) {
                        splines.push(r4(*v));
                    }
                }
            }
            "marker" => {
                // EMPIRICAL: a LightGroup's members are embedded "baked-in" light records without a position of their own (625 member
                // GUIDs, 7 light entities in ugf_lta_a_int_0001), so only their number is shown
                markers.push(json!({"c": vol["cls"], "m": rm5(m), "n": vol["members"].as_array().map_or(0, |a| a.len())}));
            }
            _ => {}
        }
    }
    let mut lboxes = vec![]; // LightBox: box (min, max) through the entity transform (the engine's light box)
    for (light, _name, m) in &stats.lights {
        if light["kind"] == "lightbox" {
            let (lo, hi) = (v3(&light["box"][0]), v3(&light["box"][1]));
            let c = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
            let h = [((hi[0] - lo[0]) / 2.0).max(1e-4), ((hi[1] - lo[1]) / 2.0).max(1e-4), ((hi[2] - lo[2]) / 2.0).max(1e-4)];
            lboxes.push(rm5(&compose(m, &[h[0], 0.0, 0.0, c[0], 0.0, h[1], 0.0, c[1], 0.0, 0.0, h[2], c[2]])));
        }
    }
    let mut mk = Counts::default();
    for x in &markers {
        mk.add(x["c"].as_str().unwrap_or(""));
    }
    let json = serde_json::to_vec(&json!({"boxes": boxes, "rooms": rooms, "shapes": shapes, "lightboxes": lboxes, "fogs": fogs,
        "actions": actions, "navs": navs, "markers": markers, "rconns": rconns, "ledges": ledges,
        "covers": covers, "dboxes": dboxes, "splines": splines}))
    .unwrap();
    let counts = json!({"boxes": boxes.len(), "rooms": rooms.len(), "shapes": nshapes, "lightboxes": lboxes.len(),
        "fogs": fogs.len(), "actions": actions.len(), "navs": nnavs, "markers": mk.to_json(), "rconns": rconns.len(),
        "ledges": nledges, "covers": ncovers, "dboxes": dboxes.len(), "splines": nsplines});
    VolOut {
        json,
        counts,
        boxes: boxes.len(),
        rooms: rooms.len(),
        shapes: nshapes,
        fogs: fogs.len(),
        actions: actions.len(),
        navs: nnavs,
        rconns: rconns.len(),
        ledges: nledges,
        covers: ncovers,
        dboxes: dboxes.len(),
        splines: nsplines,
        markers: mk,
    }
}

/// Light entities for the editor-style icons and gizmos (see `socpak::light_info`).  LightBox entities become boxes in the volumes
/// feed; EnvironmentLight becomes an icon-only 'Env' light.
fn compile_lights(stats: &Stats) -> (Vec<u8>, Counts) {
    let mut out = vec![];
    let mut counts = Counts::default();
    for (light, name, m) in &stats.lights {
        let k = light["kind"].as_str().unwrap_or("");
        if k == "lightbox" {
            continue;
        }
        let t = if k == "envlight" { "Env".to_string() } else { light["type"].as_str().unwrap_or("None").to_string() };
        counts.add(&t);
        let r = light["radius"].as_f64().unwrap_or(0.0);
        let plane = arr_f(&light["plane"]);
        let (pw, ph) = if plane.len() >= 2 { (plane[0], plane[1]) } else { (1.0, 1.0) };
        out.push(json!({
            "t": t, "n": name, "r": r4(r.min(9.9e11)), "b": r4(light["bulb"].as_f64().unwrap_or(0.0)),
            "c": if light["color"].is_array() { Value::Array(arr_f(&light["color"]).iter().map(|v| r4(*v)).collect()) } else { Value::Null },
            "k": light.get("temp").cloned().unwrap_or(Value::Null),
            "f": light.get("fov").cloned().unwrap_or(json!(90.0)),
            "w": pw, "h": ph,
            "a": light.get("active").map_or(true, |v| !matches!(v, Value::Bool(false) | Value::Null)),
            "fl": light["flare"].as_bool().unwrap_or(false),
            "m": rm5(m)
        }));
    }
    (serde_json::to_vec(&out).unwrap(), counts)
}

/// Decal render nodes (.soc chunk 0x10, type 0x10) grouped by material.  In the engine (and its SPIR-V shaders): a deferred projected decal, a box
/// of half-extent = node scale, rotated and placed by the node pose, depth-tested against the scene; alpha = (1 - |axis
/// coordinate|^AlphaFalloff) * smoothstep(AngleFadeThreshold, 1, n.axis) * texture alpha * material opacity; TexSlot1 is the diffuse
/// map.  The projection axis is not pinned down by the engine analysis; EMPIRICAL (38 decals of ugf_lta_a_int_0001 rendered with each
/// axis in a browser, pixels changed against no decals): local Z changes 4x more pixels than X or Y for every material tried, so the
/// viewer defaults to local Z.  Returns (binary, table, skipped).
fn compile_decals(stats: &Stats, reg: &mut MatRegistry) -> (Vec<u8>, Vec<Value>, usize) {
    let mut by_mat: HashMap<&str, Vec<&Mat>> = HashMap::new();
    let mut order: Vec<&str> = vec![];
    for (path, m) in &stats.decals {
        by_mat.entry(path).or_insert_with(|| {
            order.push(path);
            vec![]
        });
        by_mat.get_mut(path.as_str()).unwrap().push(m);
    }
    let (mut mids, mut mats, mut table) = (vec![], vec![], vec![]);
    for path in order {
        let gi = reg.resolve(path, 0);
        let Some(mat) = (gi >= 0).then(|| reg.materials[gi as usize].clone()) else { continue };
        if mat["tex"].as_i64().unwrap_or(-1) < 0 {
            continue; // nothing to draw (no material file, or no diffuse map in the archive)
        }
        let d = mat.get("decal").cloned().unwrap_or(json!({"falloff": 1.0, "angle": 0.0, "opac": -1}));
        table.push(json!({"path": path, "tex": mat["tex"], "opacity": mat["opacity"], "falloff": d["falloff"],
            "angle": d["angle"], "opac": d.get("opac").cloned().unwrap_or(json!(-1)), "color": mat["color"], "n": by_mat[path].len()}));
        for m in &by_mat[path] {
            mids.push(table.len() as u32 - 1);
            mats.extend(m.iter().map(|v| *v as f32));
        }
    }
    let skipped = stats.decals.len() - mids.len();
    let mut bin = (mids.len() as u32).to_le_bytes().to_vec();
    for v in &mids {
        bin.extend(v.to_le_bytes());
    }
    for v in &mats {
        bin.extend(v.to_le_bytes());
    }
    (bin, table, skipped)
}

impl<'a> Zone<'a> {
    /// Compiles a zone.  `progress` is told the phase ("collecting instances", "bounding spheres", "done").
    pub fn compile(p4k: &'a P4k, items: Option<&'a ItemMaterials>, spec: ZoneSpec, progress: &dyn Fn(&str)) -> Result<Zone<'a>, String> {
        let t0 = std::time::Instant::now();
        progress("collecting instances");
        let isrc: Option<&dyn crate::items::ItemMaterialSource> = items.map(|i| i as &dyn crate::items::ItemMaterialSource);
        let mut stats = Stats::with_max(spec.max_instances.unwrap_or(usize::MAX));
        // the interaction catalog (set only when the item records are there and it is ready)
        let catalog = items.filter(|_| !spec.no_catalog).map(|i| Catalog::new(i, p4k)).filter(|c| c.ready());
        let cat = catalog.as_ref();
        let mut wear_floor: HashMap<String, Option<(f64, f64)>> = HashMap::new(); // geometry name -> (wear, dirt) authored on a loadout item's record
        let ship_pal = spec.livery.as_ref().and_then(|l| l.palette.clone());
        let mut out: Vec<Rec> = vec![];
        let to_rec = |i: Instance| Rec {
            name: i.geometry,
            m: i.matrix,
            mtl: i.mtl,
            pal: i.pal,
            tag: i.tag,
            car: None,
            ride: i.ride.map(|r| RideRec { uid: Key::E(r.uid.0, r.uid.1), parent: Some(Key::E(r.parent.0, r.parent.1)), helper: Some(r.helper), att: true }),
        };
        let mut collected: Vec<Instance> = vec![];
        match &spec.parts {
            None => {
                let name = spec.entry.as_deref().ok_or("no container given")?;
                let entry = p4k.get(name).ok_or_else(|| format!("KeyError: {}", name))?;
                collect_instances_cat(p4k, entry, &IDENT, 0, &mut collected, &mut stats, &HashSet::new(), spec.only_soc.as_deref(), isrc, cat);
                out.extend(collected.drain(..).map(to_rec));
                // models added to a container by the caller (the loot map's crates): plain instances of the zone, so they go through the
                // zone's one material registry and are drawn by the viewer's own pipeline (layered shaders, MeshDecal, palettes).  Nothing
                // interactive (no catalog tag).  The palette is the caller's (the class's own TintPaletteRef): the branding sticker of a loot
                // crate is a STENCIL_AS_STICKER MeshDecal whose stencil map comes from it (EMPIRICAL: with it the Weapons crate turns orange
                // and gets its logo, without it a grey placeholder).
                for x in &spec.extras {
                    match gidx_get(p4k, &x.path) {
                        Some(ge) => out.push(Rec { name: ge.name.to_string(), m: x.matrix, mtl: None, pal: x.pal.clone().filter(|p| !p.is_empty()), tag: None, car: None, ride: None }),
                        None => stats.notes.push(format!("the added model {} is not in the archive", x.path)),
                    }
                }
            }
            Some(parts) => {
                for (name, matrix) in parts {
                    let entry = p4k.get(name).ok_or_else(|| format!("KeyError: {}", name))?;
                    collect_instances_cat(p4k, entry, matrix, 0, &mut collected, &mut stats, &HashSet::new(), None, isrc, cat);
                }
                out.extend(collected.drain(..).map(to_rec));
                if let Some(sp) = &ship_pal {
                    // INFERRED: the vehicle's palette is the default of every object of the ship that names none
                    for r in out.iter_mut() {
                        if r.pal.as_deref().map_or(true, str::is_empty) {
                            r.pal = Some(sp.clone());
                        }
                    }
                }
                let hull = spec.hull.as_deref().filter(|h| !h.is_empty()).and_then(|h| gidx_get(p4k, h));
                if let Some(h) = hull {
                    out.push(Rec { name: h.name.to_string(), m: IDENT, mtl: spec.livery.as_ref().and_then(|l| l.material.clone()), pal: ship_pal.clone(), tag: None, car: None, ride: None });
                } else if let Some(h) = spec.hull.as_deref().filter(|h| !h.is_empty()) {
                    stats.notes.push(format!("the ship's hull model {} is not in the archive", h));
                }
                for x in &spec.extras {
                    let Some(ge) = gidx_get(p4k, &x.path) else { continue };
                    let cls = x.cls.as_deref().filter(|c| !c.is_empty());
                    if let (Some(cls), Some(_)) = (cls, items) {
                        if !wear_floor.contains_key(ge.name.as_ref()) {
                            // per geometry entry: one model shared by items with other floors keeps the first; None = looked up, all zero
                            wear_floor.insert(ge.name.to_string(), floors_pair(itemwear::class_floors_for(items, cls, &x.path)));
                        }
                    }
                    let desc = cat.zip(cls).and_then(|(c, cl)| c.describe(cl)).filter(|d| d.kind != "ladder");
                    // uid / parent / helper: the item hangs on a helper node of its parent item (ships::loadout_items); a gun on a turret's
                    // pitch joint turns with it in the viewer, see `riders` below
                    let ride = x.uid.map(|u| RideRec { uid: Key::U(u), parent: x.parent.map(Key::U), helper: x.helper.clone(), att: false });
                    let tag = desc.map(|d| Tag { cls: cls.unwrap().to_string(), name: x.port.clone(), d, scope: None, guid: None, links: vec![], uid: x.uid, parent: x.parent });
                    let ipal = x.pal.clone().filter(|p| !p.is_empty()).or_else(|| ship_pal.clone()); // an item's own palette first
                    out.push(Rec { name: ge.name.to_string(), m: x.matrix, mtl: None, pal: ipal, tag, car: None, ride });
                }
            }
        }
        if items.is_some() {
            // entities of the containers: the class of the first entity that places each model
            for (gname, (gcls, gpath)) in &stats.geo_cls {
                if !wear_floor.contains_key(gname) {
                    wear_floor.insert(gname.clone(), floors_pair(itemwear::class_floors_for(items, gcls, gpath)));
                }
            }
        }
        // elevator cars (`elevator`): the car container of every elevator, drawn at its starting stop
        let mut cars: Vec<Car> = if spec.no_catalog { vec![] } else { place_cars(p4k, &mut out, &mut stats, isrc, cat) };
        let overlay = std::mem::take(&mut stats.overlay);
        let src = GeoSource::with_overlay(p4k, &overlay);
        let mut registry = MatRegistry::with_options(p4k, 256, true, true);
        registry.items = isrc;
        progress("bounding spheres");

        // A render node may carry its own material (replacing the cgf's) and a tint palette.  A geometry file with an override that
        // differs from the file's own material becomes its own geometry entry; the palette is a per-instance slot.
        let own: Mutex<HashMap<String, Option<String>>> = Mutex::new(HashMap::new());
        let zone_stub = OwnMaterial { src, items: isrc, cache: &own };

        // seat access entities of the ship decide which of the Deploy / Retract clips play for each seat (adb.rs skip states)
        let mut port_skip: HashMap<String, Vec<(adb::Skip, adb::Skip)>> = HashMap::new();
        if let Some(cat) = cat {
            for x in &spec.extras {
                if let Some(c) = x.cls.as_deref().filter(|c| !c.is_empty()) {
                    for (port, es, xs) in cat.access_skips(c) {
                        port_skip.entry(port.to_lowercase()).or_default().push((es, xs));
                    }
                }
            }
        }
        // an entity hung on a helper of another entity of its container (a sign on a door, ParentEntityCryGUID) is placed after its parent
        out.sort_by_key(|r| r.ride.as_ref().map_or(false, |x| x.att));
        let mut by_key: HashMap<GKey, Vec<(usize, Option<Link>)>> = HashMap::new();
        let mut pal_count = Counts::default();
        // interactive entities (doors, seats, turrets): see interact.rs and inter.rs
        let mut tagged: Vec<Tagged> = vec![];
        let mut ents_by_uid: HashMap<Key, usize> = HashMap::new(); // loadout item uid / entity key -> its split entity
        let mut rider_root: HashMap<Key, Option<(usize, usize)>> = HashMap::new(); // uid -> (turret entity, joint) of items that ride a turret joint
        let (mut static_doors, mut static_guids) = (0usize, HashSet::<(usize, String)>::new());
        for (i, r) in out.iter().enumerate() {
            let mut tag = r.tag.clone();
            if let Some(t) = tag.as_mut() {
                if matches!(t.d.kind, "seat" | "turret") {
                    if let Some(pairs) = t.name.as_deref().filter(|n| !n.is_empty()).and_then(|n| port_skip.get(&n.to_lowercase())).filter(|p| !p.is_empty()) {
                        // INFERRED: entrances of one seat differ now and then (MPUV): the commonest pair (the first of equals)
                        let mut best = &pairs[0];
                        let mut best_n = pairs.iter().filter(|p| *p == best).count();
                        for p in &pairs[1..] {
                            let n = pairs.iter().filter(|q| *q == p).count();
                            if n > best_n {
                                best = p;
                                best_n = n;
                            }
                        }
                        let mut d: Desc = (*t.d).clone();
                        d.ent = Some(best.clone());
                        t.d = Arc::new(d);
                    }
                }
            }
            let clink = r.car.map(|c| Link { to: LinkTo::Car(c), gi: None });
            let ov = zone_stub.material_override(&r.name, r.mtl.as_deref());
            if let Some(p) = r.pal.as_deref().filter(|p| !p.is_empty()) {
                pal_count.add(p);
            }
            let dg: Option<Arc<Dg>> = match (&tag, cat) {
                (Some(t), Some(c)) if t.d.kind == "door" => c.door_groups(&src, &r.name),
                (Some(t), Some(c)) if matches!(t.d.kind, "seat" | "turret" | "aim") => c.seat_groups(&src, &r.name, &t.d), // a seat whose model has a chair part that moves, or a turret's rotating joints
                _ => None,
            };
            if let Some(dg) = dg {
                // a door (or seat) with an animation: its moving parts become instances of their own (same matrix), the rest stays
                let t = tag.unwrap();
                let ei = tagged.len();
                if let Some(rd) = &r.ride {
                    ents_by_uid.insert(rd.uid.clone(), ei);
                }
                if let Some(g) = t.guid.as_deref().filter(|g| !g.is_empty()) {
                    // container entities that carry attachments name their parent by GUID (scope = the container instance)
                    ents_by_uid.insert(Key::E(t.scope.unwrap_or(0), g.to_lowercase()), ei);
                }
                for (gi, grp) in dg.groups.iter().enumerate() {
                    let sub = Some(SubSpec { inside: true, nodes: grp.nodes.clone() });
                    by_key.entry((r.name.clone(), ov.clone(), sub)).or_default().push((i, Some(Link { to: LinkTo::Leaves(ei), gi: Some(gi) })));
                }
                if dg.rest.as_ref().map_or(false, |v| !v.is_empty()) {
                    let sub = Some(SubSpec { inside: false, nodes: dg.moving.clone() });
                    by_key.entry((r.name.clone(), ov.clone(), sub)).or_default().push((i, clink));
                }
                tagged.push(Tagged { tag: t, m: r.m, geo: r.name.clone(), dg: Some(dg), leaves: vec![], car: r.car, riders: vec![] });
                continue;
            }
            if let Some(t) = tag {
                if t.d.kind == "door" {
                    static_doors += 1;
                    if let Some(k) = door_key(&t) {
                        static_guids.insert(k); // a panel linked to it can only say so
                    }
                } else {
                    tagged.push(Tagged { tag: t, m: r.m, geo: r.name.clone(), dg: None, leaves: vec![], car: r.car, riders: vec![] });
                }
            }
            let mut link = clink;
            if let Some(rd) = &r.ride {
                if let Some(parent) = &rd.parent {
                    // an item on a turret joint (a gun on the pitch joint) follows that joint
                    let mut root = rider_root.get(parent).copied().flatten();
                    if root.is_none() {
                        root = ents_by_uid.get(parent).and_then(|&e| {
                            let j = tagged[e].dg.as_ref().and_then(|dg| dg.jof.get(&rd.helper.clone().unwrap_or_default().to_lowercase()).copied());
                            j.map(|j| (e, j))
                        });
                    }
                    rider_root.insert(rd.uid.clone(), root);
                    if let Some((e, j)) = root {
                        link = Some(Link { to: LinkTo::Riders(e), gi: Some(j) });
                    }
                }
            }
            by_key.entry((r.name.clone(), ov.clone(), None)).or_default().push((i, link));
        }
        let slots: HashMap<&str, usize> = pal_count.sorted_desc().into_iter().take(PAL_SLOTS - 1).enumerate().map(|(i, (p, _))| (p, i + 1)).collect();
        let mut pal_slots: Vec<&str> = slots.keys().copied().collect();
        pal_slots.sort_by_key(|p| slots[p]);
        let pal_slots: Vec<String> = pal_slots.into_iter().map(str::to_string).collect();

        // bounding spheres, one per distinct geometry file, in parallel
        let mut keys: Vec<GKey> = by_key.keys().cloned().collect();
        // sorted by (name, override or "", Python repr of sub): "('in', ..." sorts before "None"
        keys.sort_by(|a, b| {
            (a.0.as_str(), a.1.as_deref().unwrap_or(""), SubSpec::repr(&a.2)).cmp(&(b.0.as_str(), b.1.as_deref().unwrap_or(""), SubSpec::repr(&b.2)))
        });
        let mut names: Vec<&str> = keys.iter().map(|k| k.0.as_str()).collect();
        names.dedup();
        let sphere_res: HashMap<&str, Result<Option<Sphere>, String>> = names.par_iter().map(|n| (*n, compile_sphere(&src, n))).collect();

        let mut geoms: Vec<Geom> = vec![];
        let mut gid_of: HashMap<GKey, usize> = HashMap::new();
        let mut bad = 0usize;
        let mut counted: HashSet<&str> = HashSet::new();
        for key in &keys {
            let (name, ov, sub) = (&key.0, &key.1, &key.2);
            let sphere = match &sphere_res[name.as_str()] {
                Ok(s) => *s,
                Err(_) => {
                    // an unreadable cgf still gets instances (drawn once its mesh loads, or never)
                    if counted.insert(name) {
                        bad += 1;
                    }
                    None
                }
            };
            let n = by_key[key].len();
            let wf = |nm: &str| -> Option<(f64, f64)> {
                wear_floor.get(nm).copied().flatten().or_else(|| lod_base(nm).and_then(|b| wear_floor.get(&b).copied().flatten()))
            };
            let mut register = |nm: &str, count: usize, parent: i64, geoms: &mut Vec<Geom>| -> usize {
                let (c, r) = sphere.unwrap_or(([0.0; 3], 10.0));
                gid_of.insert((nm.to_string(), ov.clone(), sub.clone()), geoms.len());
                // "p": the base geometry of a LOD file (collision comes from the base)
                geoms.push(Geom {
                    name: nm.to_string(),
                    c: [rnd(c[0], 4), rnd(c[1], 4), rnd(c[2], 4)],
                    r: rnd(r, 4),
                    lods: vec![],
                    n: count,
                    p: parent,
                    mtl: ov.clone(),
                    wear: wf(nm), // LOD files wear like their base file
                    sub: sub.clone(),
                });
                geoms.len() - 1
            };
            let g = register(name, n, -1, &mut geoms);
            let dot = name.rfind('.').unwrap_or(name.len());
            let (stem, ext) = (&name[..dot], &name[dot + 1..]);
            for k in 1..=LOD_MAX {
                let lod = format!("{}_lod{}.{}", stem, k, ext);
                if !src.contains(&lod) {
                    break;
                }
                let lg = register(&lod, 0, g as i64, &mut geoms);
                geoms[g].lods.push(lg);
            }
        }
        // the geometry entries a livery's material applies to (the hull model and its LOD files): they can be rebuilt with another
        // material without recompiling, see livery_override / livery_geometry.  `hull_shared`: other instances of the hull file use the
        // same entry.
        let (mut livery_gids, mut hull_shared) = (vec![], false);
        if spec.parts.is_some() {
            if let Some(h) = spec.hull.as_deref().filter(|h| !h.is_empty()) {
                if let Some(hg) = gidx_get(p4k, h) {
                    let ov = zone_stub.material_override(&hg.name, spec.livery.as_ref().and_then(|l| l.material.as_deref()));
                    let hkey: GKey = (hg.name.to_string(), ov, None);
                    if let Some(&g) = gid_of.get(&hkey) {
                        livery_gids = std::iter::once(g).chain(geoms[g].lods.iter().copied()).collect();
                        hull_shared = by_key[&hkey].len() > 1;
                    }
                }
            }
        }
        let mut by_name_count: HashSet<&str> = HashSet::new();
        for k in &keys {
            by_name_count.insert(&k.0);
        }
        let mut order: Vec<&GKey> = keys.iter().collect();
        order.sort_by_key(|k| gid_of[*k]);
        let total = out.len();
        let (mut ids, mut mats, mut pals) = (Vec::with_capacity(4 + total * 4), Vec::with_capacity(total * 48), Vec::with_capacity(total));
        let (mut mn, mut mx) = ([1e30f64; 3], [-1e30f64; 3]);
        let mut n_inst = 0usize;
        for key in &order {
            let gid = gid_of[*key];
            let g = &geoms[gid];
            let (gc, gr) = (g.c, g.r);
            for &(ri, link) in &by_key[*key] {
                let r = &out[ri];
                if let Some(l) = link {
                    let leaf = Leaf { i: n_inst, gi: l.gi };
                    match l.to {
                        LinkTo::Leaves(e) => tagged[e].leaves.push(leaf),
                        LinkTo::Riders(e) => tagged[e].riders.push(leaf),
                        LinkTo::Car(c) => cars[c].leaves.push(leaf),
                    }
                }
                ids.extend((gid as u32).to_le_bytes());
                for v in &r.m {
                    mats.extend((*v as f32).to_le_bytes());
                }
                pals.push(r.pal.as_deref().and_then(|p| slots.get(p)).copied().unwrap_or(0) as u8);
                n_inst += 1;
                let c = ivo::xform(&r.m, gc);
                let s = (0..3).map(|j| (0..3).map(|i| r.m[i * 4 + j].powi(2)).sum::<f64>().sqrt()).fold(f64::MIN, f64::max);
                for k in 0..3 {
                    mn[k] = mn[k].min(c[k] - gr * s);
                    mx[k] = mx[k].max(c[k] + gr * s);
                }
            }
        }
        if mn[0] > mx[0] {
            mn = [-1.0; 3];
            mx = [1.0; 3];
        }
        let mut inst = (n_inst as u32).to_le_bytes().to_vec();
        inst.extend(ids);
        inst.extend(mats);
        inst.extend(pals); // + one palette slot byte per instance
        let (decals, decal_table, decal_skipped) = compile_decals(&stats, &mut registry);
        let vol = compile_volumes(&stats);
        let (lights, light_counts) = compile_lights(&stats);
        // the interaction feed (k=inter) and the elevator feed (k=elev); without a catalog they are the empty documents
        let mut inter_json = br#"{"items":[],"models":[],"ladders":[],"turrets":[],"opsets":[],"approx":""}"#.to_vec();
        let mut elev_json = br#"{"elevators":[]}"#.to_vec();
        let mut inter_notes: Vec<String> = vec![];
        let mut panel_geos: HashMap<String, bool> = HashMap::new();
        if let Some(cat) = cat {
            let panel_style = PanelStyle::new(p4k);
            let mut ctx = Ctx {
                cat,
                src: &src,
                registry: &mut registry,
                own: &zone_stub,
                panel_style: &panel_style,
                hull: spec.hull.as_deref(),
                title: spec.title.as_deref(),
                ship_cmds: &spec.ship_cmds,
                panel_geos: &mut panel_geos,
            };
            let io = compile_interactables(&mut ctx, &tagged, &stats.ladders, &stats.links, static_doors, &static_guids);
            inter_json = io.json;
            inter_notes = io.notes;
            let (doc, en) = compile_elevators(&io.items, &tagged, &cars);
            if let Some(d) = doc {
                elev_json = d;
            }
            inter_notes.extend(en);
        }
        let mut palettes: Vec<(&str, usize)> = stats.palette_order.iter().map(|p| (p.as_str(), stats.palettes[p])).collect();
        palettes.sort_by(|a, b| b.1.cmp(&a.1));

        let n_lods: usize = geoms.iter().map(|g| g.lods.len()).sum();
        let mut notes = vec![
            format!("{} instances, {} distinct geometry files (+{} LOD files), compiled in {:.0} s", n_inst, by_name_count.len(), n_lods, t0.elapsed().as_secs_f64()),
            "geometry and textures are fetched when the camera needs them; LOD file choice by distance is a viewer heuristic, not the engine's rule".to_string(),
            DRAWN_NOTE.to_string(),
        ];
        if let Some(s) = &spec.only_soc {
            notes.insert(0, format!("showing only {} of this container", s));
        }
        if stats.truncated {
            notes.push(format!("truncated at {} instances (the ?max= limit in the URL)", stats.max));
        }
        notes.extend(spec.extra_notes.iter().cloned());
        notes.extend(inter_notes);
        if stats.class_geometry > 0 {
            notes.push(format!(
                "{} entities had no model path of their own: their model comes from the DataCore record of their entity class (Game2.dcb, SGeometryResourceParams), and they are drawn",
                stats.class_geometry
            ));
        }
        if stats.no_geometry > 0 {
            notes.push(no_geometry_note(&stats, 6));
        }
        if !stats.missing_geometry.is_empty() {
            notes.push(missing_geometry_note(&stats, 3));
        }
        if bad > 0 {
            notes.push(format!("{} geometry files could not be parsed for a bounding sphere (assumed 10 m)", bad));
        }
        if stats.missing_containers > 0 || stats.skipped_containers > 0 {
            notes.push(format!("nested containers: {} not found in the archive, {} skipped (depth / cycle)", stats.missing_containers, stats.skipped_containers));
        }
        notes.extend(summarise(&stats.notes));
        if !light_counts.order.is_empty() {
            notes.push(format!("lights: {}", light_counts.sorted_desc().iter().map(|(t, n)| format!("{} {}", n, t)).collect::<Vec<_>>().join(", ")));
        }
        if vol.boxes + vol.rooms + vol.shapes + vol.fogs + vol.actions + vol.navs + vol.rconns + vol.ledges + vol.covers + vol.dboxes + vol.splines > 0
            || !vol.markers.order.is_empty()
        {
            // any of boxes, rooms, shapes, fogs, actions, navs, rconns, ledges, covers, dboxes, splines is non-zero, or markers
            notes.push(format!(
                "volumes (off by default): {} area boxes, {} area shapes, {} rooms (rooms store no volume: a sphere of the entity radius is shown)",
                vol.boxes, vol.shapes, vol.rooms
            ));
            let mk = serde_json::to_string(&vol.markers.to_json()).unwrap();
            notes.push(format!(
                "also off by default: {} fog volumes, {} action areas, {} navigation areas, {} room connectors, {} ledges, {} cover surfaces, {} landing / hangar area boxes, {} splines, markers {}",
                vol.fogs, vol.actions, vol.navs, vol.rconns, vol.ledges, vol.covers, vol.dboxes, vol.splines, py_dict_repr(&vol.markers)
            ));
            let _ = mk;
        }
        if !decal_table.is_empty() {
            notes.push(format!("{} decals in {} materials (projected boxes; {} skipped for lack of a texture)", (decals.len() - 4) / 52, decal_table.len(), decal_skipped));
        }
        let lv = spec.livery.as_ref();
        let info = json!({
            "asset": spec.title.clone().or_else(|| spec.entry.clone()).unwrap_or_default(),
            "instances": n_inst,
            "geoms": geoms.iter().map(Geom::to_json).collect::<Vec<_>>(),
            "bbox": [mn, mx],
            "frames": ZONE_FRAMES, // geo blobs carry the file's own tangent frame after the indices when their header has "tn"
            "decals": decal_table, "volumes": vol.counts, "lights": light_counts.to_json(),
            "palettes": palettes.iter().map(|(p, n)| json!({"path": p, "n": n})).collect::<Vec<_>>(),
            "palslots": pal_slots,
            "livgids": livery_gids, "livshared": hull_shared,
            "livery": match lv { Some(l) => json!({"id": l.id, "palette": ship_pal, "material": l.material}), None => Value::Null },
            "places": spec.parts.iter().flatten().map(|(e, m)| {
                let base = e.rsplit('/').next().unwrap();
                json!({"name": base.rsplit_once('.').map_or(base, |x| x.0), "pos": [rnd(m[3], 3), rnd(m[7], 3), rnd(m[11], 3)]})
            }).collect::<Vec<_>>(),
            "notes": notes, "errors": stats.errors.iter().take(40).collect::<Vec<_>>(),
        });
        let info = serde_json::to_vec(&info).map_err(|e| e.to_string())?;
        progress("done");
        let own_cache = own.into_inner().unwrap();
        Ok(Zone {
            p4k,
            items,
            spec,
            overlay,
            registry: Mutex::new(registry),
            texhost: TextureHost::new(p4k),
            geoms: RwLock::new(geoms),
            gid_of: Mutex::new(gid_of),
            own: Mutex::new(own_cache),
            info,
            inst,
            decals,
            volumes: vol.json,
            lights,
            inter: inter_json,
            elev: elev_json,
            panel_geos,
            dash: OnceLock::new(),
            pal_slots,
            livery_gids,
            geo_cache: Mutex::new(HashMap::new()),
            liv_cache: Mutex::new(HashMap::new()),
            tex_lock: Mutex::new(()),
            instance_count: n_inst,
        })
    }

    fn src(&self) -> GeoSource<'_> {
        GeoSource::with_overlay(self.p4k, &self.overlay)
    }

    // ------------------------------------------------------------------ served data
    pub fn info(&self) -> &[u8] {
        &self.info
    }

    /// Binary: u32 N, u32 gid[N], f32 mat[N][12] (row-major 3x4), u8 palette slot[N]; sorted by gid.
    pub fn instances(&self) -> &[u8] {
        &self.inst
    }

    /// Binary: u32 N, u32 material[N] (index into info.decals), f32 matrix[N][12].
    pub fn decals(&self) -> &[u8] {
        &self.decals
    }

    /// JSON {boxes, rooms, shapes, lightboxes, fogs, actions, navs, markers, rconns, ledges, covers, dboxes, splines}.
    pub fn volumes(&self) -> &[u8] {
        &self.volumes
    }

    /// JSON list of {t: type, n: name, r: radius, b: bulb radius, c: [r, g, b] | null, k: kelvin | null, f: FOV deg, w / h: plane
    /// size, a: active, fl: flare, m: world matrix (12)}.
    pub fn lights(&self) -> &[u8] {
        &self.lights
    }

    /// JSON `{items, models, ladders, turrets, opsets, approx}` for `k=inter`: doors ({leaves: [{i: instance, g: group}], m: model, t:
    /// open seconds, tc: close seconds, open}), `models[m]` = {joints, groups}, seats and turret seats ({eye, yaw, exit, lim}), panels,
    /// ladders; all in compile (world) coordinates.  `approx` is a note when some door uses the approximate slide, else ''.
    pub fn interactions(&self) -> &[u8] {
        &self.inter
    }

    /// JSON `{elevators: [{id, name, cab, source, at, leaves: [instance index], doors: [door item ids inside the car], stops: [{name,
    /// label, m (12 floats), gate, calls}], legs (n x n {t, fa}), box (rider volume in car-local coordinates or null), panel (car-local
    /// position of the interior panel), accel, speed, slide}]}` for `k=elev`; world (compile) coordinates.
    pub fn elevators(&self) -> &[u8] {
        &self.elev
    }

    /// The spec this zone was compiled from.
    pub fn spec(&self) -> &ZoneSpec {
        &self.spec
    }

    pub fn instance_count(&self) -> usize {
        self.instance_count
    }

    pub fn geom_count(&self) -> usize {
        self.geoms.read().unwrap().len()
    }

    pub fn geom(&self, gid: usize) -> Option<Geom> {
        self.geoms.read().unwrap().get(gid).cloned()
    }

    pub fn livery_gids(&self) -> &[usize] {
        &self.livery_gids
    }

    pub fn pal_slots(&self) -> &[String] {
        &self.pal_slots
    }

    /// Number of materials / textures the registry has handed out so far.
    pub fn registry_sizes(&self) -> (usize, usize) {
        let r = self.registry.lock().unwrap();
        (r.materials.len(), r.textures.len())
    }

    /// Runs `f` on the registry (read only); used by the disk cache to fingerprint the ids handed out so far.
    pub fn with_registry<R>(&self, f: impl FnOnce(&MatRegistry) -> R) -> R {
        f(&self.registry.lock().unwrap())
    }

    /// The registry's id-bearing state (`MatRegistry::state_json`), taken between two geometry builds.
    pub fn registry_state(&self) -> Value {
        let _g = self.tex_lock.lock().unwrap();
        self.registry.lock().unwrap().state_json()
    }

    /// Adopts a stored registry state that extends the live one (`MatRegistry::adopt_state`); `Err` leaves the registry untouched.
    /// Builds are held off meanwhile, so no blob is made against a half-adopted registry.
    pub fn registry_adopt(&self, st: &Value) -> Result<(), String> {
        let _g = self.tex_lock.lock().unwrap();
        self.registry.lock().unwrap().adopt_state(st)
    }

    /// The registry's texture entry `tid` (`{path, chan, wear}`), if any.
    pub fn texture_entry(&self, tid: usize) -> Option<Value> {
        self.registry.lock().unwrap().textures.get(tid).cloned()
    }

    /// JSON of one tint palette (`ItemMaterials::palette`), or None.  The palette's stencil map becomes a registry texture (append
    /// only, served like any other: `k=tex&id=`).
    pub fn palette(&self, path: &str) -> Option<Vec<u8>> {
        let mut pal = self.items?.palette(path)?;
        if let Some(tex) = pal.get("decal").and_then(|d| d.get("tex")).and_then(Value::as_str).map(str::to_string) {
            let tid = self.registry.lock().unwrap().texture(&tex, "");
            pal["decal"]["tid"] = json!(tid);
        }
        serde_json::to_vec(&pal).ok()
    }

    pub fn palette_names(&self) -> Vec<u8> {
        serde_json::to_vec(&self.items.map_or(vec![], |i| i.palette_names())).unwrap()
    }

    /// The layered shader's hue table as served bytes (see `texserve::hue_lut`).
    pub fn hue_lut(&self) -> Option<Vec<u8>> {
        self.texhost.lut()
    }

    /// (content type, bytes) for registry texture `tid`, or None.  `accept` is the set of compressed families the browser has
    /// ('s3tc', 'bptc').  Raw reply: u32 header length, header JSON {fmt, w, h, mips: [[w, h, size]]}, then the mips largest first,
    /// down to the last one whose sides are multiples of 4.  A miss is not cached: the registry grows as geometry is built.
    pub fn texture(&self, tid: usize, accept: &[&str], size: usize) -> Option<texserve::Served> {
        let e = TexRef::from_json(&self.texture_entry(tid)?)?;
        self.texhost.texture(&e, accept, size)
    }

    // ------------------------------------------------------------------ geometry
    /// Mesh blob and collision blob of geometry `gid`, built on the first request (the registry hands out material and texture ids in
    /// request order, so the blobs are only meaningful together with this zone).
    pub fn blobs(&self, gid: usize) -> Result<Arc<GeoBlobs>, String> {
        if let Some(b) = self.geo_cache.lock().unwrap().get(&gid) {
            return Ok(b.clone());
        }
        let mtl = self.geom(gid).ok_or("no such geometry")?.mtl;
        let _g = self.tex_lock.lock().unwrap(); // builds are serialised on the registry
        if let Some(b) = self.geo_cache.lock().unwrap().get(&gid) {
            return Ok(b.clone());
        }
        let b = Arc::new(self.build_geometry(gid, mtl.as_deref())?);
        self.geo_cache.lock().unwrap().insert(gid, b.clone());
        Ok(b)
    }

    pub fn geometry(&self, gid: usize) -> Result<Arc<GeoBlobs>, String> {
        self.blobs(gid)
    }

    /// Binary collision / proxy meshes of a geometry (built together with its render mesh), or None when it has none.  Layout: u32
    /// header length, header JSON {nv, ni, idx16, ranges: [[kind, first index, count, optional class]]}, pad to 4, f32 [x y z] per
    /// vertex, u16 / u32 indices.  Kinds: 'proxy' (render nodes that look like collision proxies) and the collision shape kinds of
    /// builder.rs.
    pub fn collision(&self, gid: usize) -> Result<Option<Vec<u8>>, String> {
        Ok(self.blobs(gid)?.col.clone())
    }

    /// Mesh blob and collision blob of a geometry entry; `mtl` replaces the entry's compiled material override.
    fn build_geometry(&self, gid: usize, mtl: Option<&str>) -> Result<GeoBlobs, String> {
        let ge = self.geom(gid).ok_or("no such geometry")?;
        let (name, sub) = (ge.name, ge.sub);
        let mut reg = self.registry.lock().unwrap();
        let opts = Options { max_render_tris: Some(ZONE_RENDER_TRIS), mtl_override: mtl.map(str::to_string), ..Options::default() };
        let (info, mut parts) = match build_scene_named(&self.src(), &name, &opts, &mut reg) {
            Ok(x) => x,
            Err(e) if e.starts_with("not an #ivo file") => {
                // Legacy 'CrCh' brushes (see ivo::NotIvo) cannot be read: answer with an empty mesh and no collision, a deliberate and
                // permanent result, instead of a 500 that clients retry.
                let mut head = serde_json::to_vec(&json!({"nv": 0, "ni": 0, "idx16": true, "stride": 24, "ranges": [], "mats": {}, "notes": [e]})).unwrap();
                pad4(&mut head);
                return Ok(GeoBlobs { mesh: with_len(head, &[]), col: None });
            }
            Err(e) => return Err(e),
        };
        if let Some(sub) = &sub {
            // one moving part of a door (inside) or what stays (outside): see interact.rs
            let keep: HashSet<i64> = sub.nodes.iter().map(|n| *n as i64).collect();
            parts.retain(|pt| keep.contains(&pt.node) == sub.inside);
        }
        let mut by_mat: BTreeMap<i64, Vec<&builder::Part>> = BTreeMap::new();
        for pt in &parts {
            let m = pt.mat.unwrap_or(-1);
            let hide = m >= 0 && reg.materials[m as usize].get("hide").map_or(false, truthy); // materials.is_rtt_text
            if pt.kind == "render" && !hide {
                by_mat.entry(m).or_default().push(pt);
            }
        }
        let mut vbuf: Vec<u8> = vec![];
        let (mut ranges, mut mats): (Vec<Value>, Map<String, Value>) = (vec![], Map::new());
        let mut idx: Vec<u32> = vec![];
        let mut nv = 0u32;
        let (mut tn, mut have_tn): (Vec<u8>, bool) = (vec![], false);
        // the dashboard of a seat model / a panel with a display: its render-target screens get an MFD role (`mfd`)
        let base = lod_base(&name).unwrap_or_else(|| name.clone());
        let gkey = mfd::geo_key(&base);
        let dash_all = self.dash.get_or_init(|| self.items.map(mfd::dashboard_geometries).unwrap_or_default());
        let mut dash: Option<Value> = self.items.and_then(|_| dash_all.get(&gkey).cloned());
        let mut dash_role: Option<String> = dash.as_ref().and_then(|d| d["role"].as_str().map(str::to_string));
        if dash_role.is_none() && self.panel_geos.get(&gkey).copied().unwrap_or(false) {
            // a door / elevator panel with a display: panel_display
            dash = Some(json!({"screens": {}, "theme": "generic", "mfr": ""}));
            dash_role = Some("panel".to_string());
        }
        for (mid, pts) in &by_mat {
            let screen = dash_role.is_some() && *mid >= 0 && reg.materials[*mid as usize].get("rtt").and_then(Value::as_str) == Some("screen");
            // a seat dashboard's screens share one material: one range per screen node, so each is its own MFD
            let mut groups: Vec<(Option<&str>, Vec<&builder::Part>)> = vec![];
            for pt in pts {
                let k = if screen { Some(pt.name.as_str()) } else { None };
                match groups.iter_mut().find(|g| g.0 == k) {
                    Some(g) => g.1.push(pt),
                    None => groups.push((k, vec![*pt])),
                }
            }
            for (node, pts) in &groups {
            let first = idx.len();
            for pt in pts {
                let n = pt.verts.len();
                for i in 0..n {
                    let v = pt.verts[i];
                    let t = pt.uv.get(i).copied().filter(|_| !pt.uv.is_empty()).unwrap_or([0.0, 0.0]);
                    let c = if pt.vc.is_empty() { [0xFF; 4] } else { pt.vc.get(i).copied().unwrap_or([0xFF; 4]) };
                    for x in [v[0], v[1], v[2], t[0], t[1]] {
                        vbuf.extend((x as f32).to_le_bytes());
                    }
                    vbuf.extend(c);
                }
                if ZONE_FRAMES {
                    if !pt.nrm.is_empty() && !pt.tan.is_empty() && pt.nrm.len() == n {
                        have_tn = true;
                        for (nr, ta) in pt.nrm.iter().zip(pt.tan.iter()) {
                            tn.extend(pack_frame(*nr, *ta));
                        }
                        for _ in pt.nrm.len().min(pt.tan.len())..n {
                            tn.extend([0u8; 8]); // pad when there are fewer tangents than normals or vertices
                        }
                    } else {
                        tn.extend(vec![0u8; 8 * n]);
                    }
                }
                for tri in &pt.tris {
                    idx.extend([nv + tri[0], nv + tri[1], nv + tri[2]]);
                }
                nv += n as u32;
            }
            let mut rg = vec![json!(first), json!(idx.len() - first), json!(mid)];
            if screen {
                // node name, default page and kind ('mfd' paginated | 'fixed' one fixed page | '' static): mfd::screen_kinds
                let nd = node.unwrap_or("");
                if dash_role.as_deref() == Some("panel") {
                    rg.extend([json!(nd), json!(""), json!("panel")]);
                } else {
                    let screens = dash.as_ref().and_then(|d| d["screens"].as_object()).cloned().unwrap_or_default();
                    let ke = mfd::node_kind(&screens, nd);
                    let kind = ke.as_ref().map(|k| k["kind"].as_str().unwrap_or("").to_string());
                    rg.extend([
                        json!(nd),
                        json!(ke.as_ref().map_or("", |k| k["page"].as_str().unwrap_or(""))),
                        json!(match kind.as_deref() {
                            Some("instrument") | None => "",
                            Some("mfd") => "mfd",
                            Some(_) => "fixed",
                        }),
                    ]);
                }
            }
            ranges.push(Value::Array(rg));
            }
            if *mid >= 0 {
                let m = &reg.materials[*mid as usize];
                let mut o = Map::new();
                for k in ["color", "emissive", "opacity", "alpha_test", "tex"] {
                    o.insert(k.into(), m[k].clone());
                }
                for k in ["layered", "emit", "md"] {
                    if let Some(v) = m.get(k) {
                        o.insert(k.into(), v.clone());
                    }
                }
                if m.get("vcol").map_or(false, truthy) {
                    o.insert("vcol".into(), json!(true));
                    o.insert("vmode".into(), m.get("vmode").cloned().unwrap_or(json!(1)));
                }
                for k in ["maps", "gl", "mfx"] {
                    if let Some(v) = m.get(k) {
                        o.insert(k.into(), v.clone());
                    }
                }
                if m.get("trans").map_or(false, truthy) {
                    o.insert("trans".into(), json!(true)); // always blended whatever AlphaTest says (materials.is_translucent)
                }
                if let Some(v) = m.get("fx") {
                    o.insert("fx".into(), v.clone()); // shader family drawn by the closest program (materials.SHADER_FX)
                }
                if let (Some(rtt), Some(role), Some(d)) = (m.get("rtt"), &dash_role, &dash) {
                    // a screen of a seat's dashboard: the viewer draws its MFD (`mfd`); others stay static
                    o.insert("rtt".into(), rtt.clone());
                    o.insert("name".into(), m["name"].clone());
                    o.insert("role".into(), json!(role));
                    o.insert("pages".into(), if role != "panel" { json!(mfd::role_pages(role)) } else { json!([]) });
                    o.insert("theme".into(), d["theme"].clone());
                    o.insert("mfr".into(), d["mfr"].clone());
                    if let Some(v) = m.get("rasp") {
                        o.insert("rasp".into(), v.clone());
                    }
                }
                mats.insert(mid.to_string(), Value::Object(o));
            }
        }
        let idx16 = nv < 65536;
        let mut hd = Map::new();
        hd.insert("nv".into(), json!(nv));
        hd.insert("ni".into(), json!(idx.len()));
        hd.insert("idx16".into(), json!(idx16));
        hd.insert("stride".into(), json!(24));
        hd.insert("ranges".into(), Value::Array(ranges));
        hd.insert("mats".into(), Value::Object(mats));
        hd.insert("notes".into(), json!(info.notes.iter().take(3).collect::<Vec<_>>()));
        let mut body = vbuf;
        if idx16 {
            for i in &idx {
                body.extend((*i as u16).to_le_bytes());
            }
        } else {
            for i in &idx {
                body.extend(i.to_le_bytes());
            }
        }
        while body.len() % 4 != 0 {
            body.push(0);
        }
        if have_tn {
            hd.insert("tn".into(), json!(body.len()));
            body.extend(tn);
        }
        let mut head = serde_json::to_vec(&Value::Object(hd)).unwrap();
        pad4(&mut head);
        let col = collision_blob(&parts);
        Ok(GeoBlobs { mesh: with_len(head, &body), col })
    }

    // ------------------------------------------------------------------ compile snapshot (disk cache)
    /// The compile's result as bytes (own format, `Zone::from_snapshot` reads it back): the served documents, the geometry table, the
    /// overlay (designer brushes read out of the containers) and the registry as the compile left it.  Taken right after the compile:
    /// geometries added later (`add_geometry`) and registry growth from geometry builds are not part of it (the cache keeps the
    /// registry separately, see zonecache.rs).
    pub fn snapshot(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.buf.extend(SNAP_MAGIC);
        w.uv(self.instance_count as u64);
        for b in [&self.info, &self.inst, &self.decals, &self.volumes, &self.lights, &self.inter, &self.elev] {
            w.bytes(b);
        }
        let mut pg: Vec<(&String, &bool)> = self.panel_geos.iter().collect();
        pg.sort();
        w.uv(pg.len() as u64);
        for (k, v) in pg {
            w.str(k);
            w.u8(*v as u8);
        }
        w.strs(&self.pal_slots);
        w.uv(self.livery_gids.len() as u64);
        for g in &self.livery_gids {
            w.uv(*g as u64);
        }
        let geoms = self.geoms.read().unwrap();
        w.uv(geoms.len() as u64);
        for g in geoms.iter() {
            w.value(&g.to_json());
        }
        let mut names: Vec<&str> = self.overlay.names().collect();
        names.sort();
        w.uv(names.len() as u64);
        for n in names {
            w.str(n);
            w.bytes(&self.overlay.get(n).unwrap().data);
        }
        w.value(&self.registry.lock().unwrap().state_json());
        w.buf
    }

    /// Rebuilds a zone from `snapshot` bytes and the spec it was compiled from; `None` on any inconsistency (the caller compiles).
    pub fn from_snapshot(p4k: &'a P4k, items: Option<&'a ItemMaterials>, spec: ZoneSpec, bytes: &[u8]) -> Option<Zone<'a>> {
        if bytes.get(..SNAP_MAGIC.len())? != SNAP_MAGIC {
            return None;
        }
        let mut r = Reader::new(&bytes[SNAP_MAGIC.len()..]);
        let instance_count = r.uv()? as usize;
        let mut docs: Vec<Vec<u8>> = vec![];
        for _ in 0..7 {
            docs.push(r.bytes()?.to_vec());
        }
        let mut panel_geos = HashMap::new();
        for _ in 0..r.count()? {
            let k = r.string()?;
            panel_geos.insert(k, r.u8()? != 0);
        }
        let pal_slots = r.strs()?;
        let mut livery_gids = vec![];
        for _ in 0..r.count()? {
            livery_gids.push(r.uv()? as usize);
        }
        let mut geoms = vec![];
        let mut gid_of: HashMap<GKey, usize> = HashMap::new();
        for i in 0..r.count()? {
            let g = Geom::from_json(&r.value()?)?;
            gid_of.insert((g.name.clone(), g.mtl.clone(), g.sub.clone()), i);
            geoms.push(g);
        }
        let mut overlay = Overlay::default();
        for _ in 0..r.count()? {
            let n = r.string()?;
            overlay.insert(n, r.bytes()?.to_vec());
        }
        let state = r.value()?;
        if !r.done() {
            return None;
        }
        let mut registry = MatRegistry::with_options(p4k, 256, true, true);
        registry.items = items.map(|i| i as &dyn crate::items::ItemMaterialSource);
        registry.adopt_state(&state).ok()?;
        let mut d = docs.into_iter();
        let mut next = || d.next();
        Some(Zone {
            p4k,
            items,
            spec,
            overlay,
            registry: Mutex::new(registry),
            texhost: TextureHost::new(p4k),
            geoms: RwLock::new(geoms),
            gid_of: Mutex::new(gid_of),
            own: Mutex::new(HashMap::new()),
            info: next()?,
            inst: next()?,
            decals: next()?,
            volumes: next()?,
            lights: next()?,
            inter: next()?,
            elev: next()?,
            panel_geos,
            dash: OnceLock::new(),
            pal_slots,
            livery_gids,
            geo_cache: Mutex::new(HashMap::new()),
            liv_cache: Mutex::new(HashMap::new()),
            tex_lock: Mutex::new(()),
            instance_count,
        })
    }

    // ------------------------------------------------------------------ geometry registered after the compile
    /// Geometry id (and its LOD ids) of the model at `path` (a record path), registering it after the compile when no instance used it
    /// yet.  Returns (id, [new geometry ids]).  None when the model is not in the archive.  Used by the loadout swap endpoint
    /// (the viewer appends the instances): a swapped-in item's model is not in the compiled instance list.
    pub fn add_geometry(&self, path: &str) -> Option<(usize, Vec<usize>)> {
        let ge = gidx_get(self.p4k, path)?;
        let name = ge.name.to_string();
        let mut gid_of = self.gid_of.lock().unwrap();
        if let Some(&g) = gid_of.get(&(name.clone(), None, None)) {
            return Some((g, vec![]));
        }
        let (c, r) = compile_sphere(&self.src(), &name).ok().flatten().unwrap_or(([0.0; 3], 10.0));
        let mut geoms = self.geoms.write().unwrap();
        let mut new = vec![];
        let mut reg = |nm: &str, parent: i64, geoms: &mut Vec<Geom>| -> usize {
            geoms.push(Geom { name: nm.to_string(), c: [rnd(c[0], 4), rnd(c[1], 4), rnd(c[2], 4)], r: rnd(r, 4), lods: vec![], n: 0, p: parent, mtl: None, wear: None, sub: None });
            gid_of.insert((nm.to_string(), None, None), geoms.len() - 1);
            new.push(geoms.len() - 1);
            geoms.len() - 1
        };
        let base = reg(&name, -1, &mut geoms);
        let dot = name.rfind('.').unwrap_or(name.len());
        for k in 1..=LOD_MAX {
            let lod = format!("{}_lod{}.{}", &name[..dot], k, &name[dot + 1..]);
            if !self.src().contains(&lod) {
                break;
            }
            let lg = reg(&lod, base as i64, &mut geoms);
            geoms[base].lods.push(lg);
        }
        Some((base, new))
    }

    /// `{geoms: entries with id >= known (what the client has not seen), inst: [{g, m, cls, port}], missing: [path]}` for the items
    /// (swaps::diff `add`) that replace default loadout items.  Items whose model is not in the archive are listed in `missing`.
    pub fn swap_instances(&self, adds: &[Value], known: usize) -> Value {
        let (mut inst, mut missing) = (vec![], vec![]);
        for a in adds {
            let g = a.get("g").and_then(Value::as_str).unwrap_or("");
            match self.add_geometry(g) {
                None => missing.push(json!(g)),
                Some((gid, _)) => inst.push(json!({"g": gid, "m": a.get("m").cloned().unwrap_or(Value::Null), "cls": a.get("cls").cloned().unwrap_or(Value::Null), "port": a.get("port").cloned().unwrap_or(Value::Null)})),
            }
        }
        let geoms: Vec<Value> = self
            .geoms
            .read()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(i, _)| *i >= known)
            .map(|(i, g)| {
                let mut v = g.to_json();
                v["id"] = json!(i);
                v
            })
            .collect();
        json!({"geoms": geoms, "inst": inst, "missing": missing})
    }

    // ------------------------------------------------------------------ liveries
    fn own_material(&self, name: &str) -> Option<String> {
        OwnMaterial { src: self.src(), items: self.items.map(|i| i as &dyn crate::items::ItemMaterialSource), cache: &self.own }.get(name)
    }

    /// The material override a render node of geometry file `name` gets for livery material `mtl` (None = the file's own material).
    /// One rule for the compile and for live livery swaps, so both give the same assignment.
    pub fn material_override(&self, name: &str, mtl: Option<&str>) -> Option<String> {
        OwnMaterial { src: self.src(), items: self.items.map(|i| i as &dyn crate::items::ItemMaterialSource), cache: &self.own }.material_override(name, mtl)
    }

    /// Material override of the hull model for a livery (None = the hull's own material).
    pub fn livery_override(&self, livery: Option<&Livery>) -> Option<String> {
        let g0 = *self.livery_gids.first()?;
        let name = self.geom(g0)?.name;
        let _ = self.own_material(&name);
        self.material_override(&name, livery.and_then(|l| l.material.as_deref()))
    }

    /// Binary mesh (as `geometry`) of a hull geometry entry (`livery_gids`) wearing `livery`'s material.  Same vertices and indices as
    /// the compiled entry (a livery never changes geometry, only the material ranges' ids and the `mats` table; INFERRED from the
    /// livery data model).  Material and texture ids come from this zone's registry, so `tex&id=` serves their maps.  Cached per
    /// (entry, material).
    pub fn livery_geometry(&self, gid: usize, livery: Option<&Livery>) -> Result<Option<Arc<Vec<u8>>>, String> {
        if !self.livery_gids.contains(&gid) {
            return Ok(None);
        }
        let ov = self.livery_override(livery);
        if ov == self.geom(gid).and_then(|g| g.mtl) {
            return Ok(Some(Arc::new(self.geometry(gid)?.mesh.clone())));
        }
        let _g = self.tex_lock.lock().unwrap();
        let key = (gid, ov.clone());
        if let Some(b) = self.liv_cache.lock().unwrap().get(&key) {
            return Ok(Some(b.clone()));
        }
        let b = Arc::new(self.build_geometry(gid, ov.as_deref())?.mesh);
        self.liv_cache.lock().unwrap().insert(key, b.clone());
        Ok(Some(b))
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map_or(true, |x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `repr` of a dict of counts: `{'a': 1, 'b': 2}` (empty: `{}`).
fn py_dict_repr(c: &Counts) -> String {
    format!("{{{}}}", c.order.iter().map(|k| format!("'{}': {}", k, c.n[k])).collect::<Vec<_>>().join(", "))
}

/// Function name -> material key of a geometry file's own material (None when it has none), cached.
struct OwnMaterial<'x, 'y> {
    src: GeoSource<'x>,
    items: Option<&'x dyn crate::items::ItemMaterialSource>,
    cache: &'y Mutex<HashMap<String, Option<String>>>,
}

impl OwnMaterial<'_, '_> {
    fn get(&self, name: &str) -> Option<String> {
        if let Some(v) = self.cache.lock().unwrap().get(name) {
            return v.clone();
        }
        let v = (|| -> Option<String> {
            let data = self.src.read(name)?.ok()?;
            let mp = material_path(&data).ok()??;
            Some(mkey(&resolve_material_path(self.src.p4k, name, Some(&mp), self.items).0.unwrap_or_default()))
        })();
        self.cache.lock().unwrap().insert(name.to_string(), v.clone());
        v
    }

    /// The material override a render node of geometry file `name` gets for material `mtl` (None = the file's own material).
    fn material_override(&self, name: &str, mtl: Option<&str>) -> Option<String> {
        let ov = mtl.filter(|m| !m.is_empty()).and_then(normalise_material_name)?;
        if self.get(name).as_deref() == Some(mkey(&ov).as_str()) {
            return None; // the same material the file already has
        }
        Some(ov)
    }
}

fn collision_blob(parts: &[builder::Part]) -> Option<Vec<u8>> {
    let mut by_kind: BTreeMap<(&str, &str), Vec<&builder::Part>> = BTreeMap::new();
    for pt in parts {
        if pt.kind != "render" && !pt.tris.is_empty() {
            by_kind.entry((pt.kind.as_str(), pt.cls.as_deref().unwrap_or(""))).or_default().push(pt);
        }
    }
    if by_kind.is_empty() {
        return None;
    }
    let (mut verts, mut idx, mut ranges, mut nv) = (Vec::<u8>::new(), Vec::<u32>::new(), Vec::<Value>::new(), 0u32);
    for ((kind, cls), pts) in &by_kind {
        let first = idx.len();
        for pt in pts {
            for v in &pt.verts {
                for x in v {
                    verts.extend((*x as f32).to_le_bytes());
                }
            }
            for tri in &pt.tris {
                idx.extend([nv + tri[0], nv + tri[1], nv + tri[2]]);
            }
            nv += pt.verts.len() as u32;
        }
        let mut r = vec![json!(kind), json!(first), json!(idx.len() - first)];
        if !cls.is_empty() {
            r.push(json!(cls)); // cls: see builder::collision_class
        }
        ranges.push(Value::Array(r));
    }
    let idx16 = nv < 65536;
    let mut head = serde_json::to_vec(&json!({"nv": nv, "ni": idx.len(), "idx16": idx16, "ranges": ranges})).unwrap();
    pad4(&mut head);
    let mut body = verts;
    for i in &idx {
        if idx16 {
            body.extend((*i as u16).to_le_bytes());
        } else {
            body.extend(i.to_le_bytes());
        }
    }
    Some(with_len(head, &body))
}

// ---------------------------------------------------------------------------------------------------------------------------
// the background job and the feed

/// One answer of the feed: HTTP status, content type, body, and whether the web layer may cache it (`Cache-Control: max-age=3600`;
/// the status feed is `no-store`).
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Arc<Vec<u8>>,
    pub cacheable: bool,
}

impl Reply {
    fn new(status: u16, ct: &'static str, body: impl Into<Vec<u8>>, cacheable: bool) -> Reply {
        Reply { status, content_type: ct, body: Arc::new(body.into()), cacheable }
    }
}

/// The parsed query of a feed request (`k=`, `id=`, `accept=`, `size=`, `name=`).
#[derive(Debug, Clone, Default)]
pub struct FeedQuery {
    pub kind: String,
    /// None when missing or not an integer (a malformed one is answered with 400 "bad id", a missing one is -1)
    pub id: Option<i64>,
    pub bad_id: bool,
    pub accept: Vec<String>,
    pub size: Option<usize>,
    pub name: String,
}

impl FeedQuery {
    /// From the raw query pairs of the URL (already percent-decoded).
    pub fn from_pairs(pairs: &[(String, String)]) -> FeedQuery {
        let get = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        let mut q = FeedQuery { kind: get("k").unwrap_or("status").to_string(), id: Some(-1), ..Default::default() };
        if let Some(v) = get("id") {
            match v.trim().parse::<i64>() {
                Ok(n) => q.id = Some(n),
                Err(_) => q.bad_id = true,
            }
        }
        q.accept = get("accept").unwrap_or("").split(',').filter(|f| *f == "s3tc" || *f == "bptc").map(str::to_string).collect();
        q.size = Some(get("size").and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(texserve::TEX_DEFAULT));
        q.name = get("name").unwrap_or("").to_string();
        q
    }
}

/// A zone compiled in a background thread, with the progress `k=status` reports.
pub struct ZoneJob {
    state: Mutex<(String, String, Option<String>)>,
    zone: OnceLock<Zone<'static>>,
    /// state the web layer hangs on the job (e.g. the zone's disk cache); lives and dies with the job
    pub aux: OnceLock<Box<dyn std::any::Any + Send + Sync>>,
}

/// Where a zone's compile snapshot is kept (the web layer's disk cache): `load` returns the stored bytes, `store` keeps new ones.
pub trait ZoneStore: Send + Sync {
    fn load(&self) -> Option<Vec<u8>>;
    fn store(&self, bytes: &[u8], build_ms: f64);
}

impl ZoneJob {
    /// Starts the compile on its own thread and returns at once.  The archive and the item records must outlive the job (`'static`: the
    /// server leaks its one `P4k` / `ItemMaterials`, or keeps them in a `static`).
    pub fn spawn(p4k: &'static P4k, items: Option<&'static ItemMaterials>, spec: ZoneSpec) -> Arc<ZoneJob> {
        Self::spawn_with(p4k, items, spec, None)
    }

    /// [`ZoneJob::spawn`] with a snapshot store: a stored snapshot replaces the compile, a fresh compile is stored (after the zone is
    /// ready, on the same thread).
    pub fn spawn_with(p4k: &'static P4k, items: Option<&'static ItemMaterials>, spec: ZoneSpec, store: Option<Arc<dyn ZoneStore>>) -> Arc<ZoneJob> {
        let job = Arc::new(ZoneJob { state: Mutex::new(("compiling".into(), String::new(), None)), zone: OnceLock::new(), aux: OnceLock::new() });
        let j = job.clone();
        std::thread::spawn(move || j.run(p4k, items, spec, store));
        job
    }

    /// Compiles on the calling thread (tests, the command line).
    pub fn run_blocking(p4k: &'static P4k, items: Option<&'static ItemMaterials>, spec: ZoneSpec) -> Arc<ZoneJob> {
        let job = Arc::new(ZoneJob { state: Mutex::new(("compiling".into(), String::new(), None)), zone: OnceLock::new(), aux: OnceLock::new() });
        job.run(p4k, items, spec, None);
        job
    }

    fn run(&self, p4k: &'static P4k, items: Option<&'static ItemMaterials>, spec: ZoneSpec, store: Option<Arc<dyn ZoneStore>>) {
        let progress = |t: &str| self.state.lock().unwrap().1 = t.to_string();
        let t0 = std::time::Instant::now();
        // a stored snapshot (rebuilt inside catch_unwind too: a damaged one must never take the server down) replaces the compile
        let restored = store.as_ref().and_then(|s| {
            let bytes = s.load()?;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Zone::from_snapshot(p4k, items, spec.clone(), &bytes))).ok().flatten();
            if r.is_none() {
                eprintln!("zone snapshot unusable: compiling");
            }
            r
        });
        let fresh = restored.is_none();
        // a panic in the compile is reported through the status instead of killing the thread silently
        let r = match restored {
            Some(z) => {
                progress("done"); // the status document of a finished compile
                Ok(Ok(z))
            }
            None => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Zone::compile(p4k, items, spec, &progress))),
        };
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if std::env::var_os("P4K_TIMING").is_some() {
            eprintln!("zone {}: {:.0} ms", if fresh { "compiled" } else { "restored from the disk cache" }, ms);
        }
        let mut st = self.state.lock().unwrap();
        match r {
            Ok(Ok(z)) => {
                let _ = self.zone.set(z);
                st.0 = "ready".into();
                drop(st);
                if let (true, Some(s), Some(z)) = (fresh, &store, self.zone.get()) {
                    s.store(&z.snapshot(), ms);
                }
            }
            Ok(Err(e)) => {
                st.2 = Some(e);
                st.0 = "error".into();
            }
            Err(_) => {
                st.2 = Some("panic in the zone compile".into());
                st.0 = "error".into();
            }
        }
    }

    /// (state, phase, error): state is "compiling" | "ready" | "error".
    pub fn status(&self) -> (String, String, Option<String>) {
        self.state.lock().unwrap().clone()
    }

    /// The `k=status` body: `{"state": "ready", "phase": "done", "error": null}` (Python `json.dumps` separators, for byte compatibility).
    pub fn status_json(&self) -> Vec<u8> {
        let (s, p, e) = self.status();
        let q = |x: &str| serde_json::to_string(x).unwrap();
        format!("{{\"state\": {}, \"phase\": {}, \"error\": {}}}", q(&s), q(&p), e.map_or("null".to_string(), |e| q(&format!("RuntimeError({})", q(&e))))).into_bytes()
    }

    pub fn zone(&self) -> Option<&Zone<'static>> {
        self.zone.get()
    }

    /// One request of the `/zonedata/<file>` feed (everything except the ship-only kinds `liveries`, `loadout`, `hull` ..., which belong
    /// to the DataCore routes, and `lv=` livery geometry: use [`Zone::livery_geometry`]).  Statuses:
    /// 409 until ready, 400 bad request / bad id, 404 no texture, 500 on a failed build, 204 for "no collision".
    pub fn serve(&self, q: &FeedQuery) -> Reply {
        if q.kind == "status" {
            return Reply::new(200, "application/json", self.status_json(), false);
        }
        let Some(z) = self.zone() else { return Reply::new(409, "text/plain", "zone not ready", false) };
        if q.bad_id {
            return Reply::new(400, "text/plain", "bad id", false);
        }
        let ident = q.id.unwrap_or(-1);
        let json = |b: &[u8]| Reply::new(200, "application/json", b, true);
        let bin = |b: &[u8]| Reply::new(200, "application/octet-stream", b, true);
        let gid = (ident >= 0 && (ident as usize) < z.geom_count()).then_some(ident as usize);
        match (q.kind.as_str(), gid) {
            ("info", _) => json(z.info()),
            ("palette", _) => match z.palette(&q.name) {
                Some(b) => json(&b),
                None => Reply::new(404, "text/plain", "no such palette", false),
            },
            ("palettes", _) => json(&z.palette_names()),
            ("huelut", _) => match z.hue_lut() {
                Some(b) => bin(&b),
                None => Reply::new(404, "text/plain", "no hue table", false),
            },
            ("lights", _) => json(z.lights()),
            ("inter", _) => json(z.interactions()),
            ("elev", _) => json(z.elevators()),
            ("volumes", _) => json(z.volumes()),
            ("decals", _) => bin(z.decals()),
            ("inst", _) => bin(z.instances()),
            ("geo", Some(g)) => match z.geometry(g) {
                Ok(b) => bin(&b.mesh),
                Err(e) => Reply::new(500, "text/plain", format!("RuntimeError({:?})", e), false),
            },
            ("col", Some(g)) => match z.collision(g) {
                Ok(Some(b)) => bin(&b),
                Ok(None) => Reply::new(204, "application/octet-stream", vec![], false),
                Err(e) => Reply::new(500, "text/plain", format!("RuntimeError({:?})", e), false),
            },
            ("tex", _) => {
                let accept: Vec<&str> = q.accept.iter().map(String::as_str).collect();
                let got = (ident >= 0).then(|| z.texture(ident as usize, &accept, q.size.unwrap_or(texserve::TEX_DEFAULT))).flatten();
                match got {
                    Some((ct, b)) => Reply { status: 200, content_type: ct, body: b, cacheable: true },
                    None => Reply::new(404, "text/plain", "no texture", false),
                }
            }
            _ => Reply::new(400, "text/plain", "bad request", false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_repr_sorts_like_python() {
        // the geometry keys sort by the Python repr of the sub tuple: "('in'..." < "('out'..." < "None"
        assert_eq!(SubSpec::repr(&None), "None");
        assert_eq!(SubSpec::repr(&Some(SubSpec { inside: true, nodes: vec![7] })), "('in', (7,))");
        assert_eq!(SubSpec::repr(&Some(SubSpec { inside: false, nodes: vec![1, 2, 3] })), "('out', (1, 2, 3))");
        assert!(SubSpec::repr(&Some(SubSpec { inside: true, nodes: vec![1] })) < SubSpec::repr(&None));
    }

    #[test]
    fn material_keys() {
        assert_eq!(mkey("\\Data\\Objects\\A\\B.mtl"), "objects/a/b");
        assert_eq!(mkey("objects/a/b"), "objects/a/b");
        assert_eq!(mkey("/DATA/x.MTL"), "x");
    }

    #[test]
    fn union_of_spheres() {
        let a = Some(([0.0; 3], 1.0));
        let b = Some(([4.0, 0.0, 0.0], 1.0));
        let (c, r) = union_sphere(a, b).unwrap();
        assert!((r - 3.0).abs() < 1e-12 && (c[0] - 2.0).abs() < 1e-12);
        assert_eq!(union_sphere(a, Some(([0.1, 0.0, 0.0], 0.2))).unwrap().1, 1.0);
        assert_eq!(union_sphere(None, b), b);
        assert_eq!(union_sphere(a, None), a);
        assert!(union_sphere(None, None).is_none());
    }

    #[test]
    fn frame_packing() {
        assert_eq!(pack_frame([1.0, -1.0, 0.0], [0.0, 0.0, 1.0, 1.0]), [127, 129, 0, 127, 0, 0, 127, 0]);
        assert_eq!(pack_frame([0.0; 3], [0.0, 0.0, 0.0, -1.0])[3], 129); // -127
        assert_eq!(pack_frame([0.5, 0.0, 0.0], [0.0; 4])[0], 64); // round(63.5) = 64 (ties to even)
    }

    #[test]
    fn feed_query() {
        let q = FeedQuery::from_pairs(&[("k".into(), "tex".into()), ("id".into(), "7".into()), ("accept".into(), "s3tc,xx,bptc".into())]);
        assert_eq!((q.kind.as_str(), q.id, q.accept.len(), q.size), ("tex", Some(7), 2, Some(1024)));
        assert!(FeedQuery::from_pairs(&[("id".into(), "x".into())]).bad_id);
        assert_eq!(FeedQuery::from_pairs(&[]).kind, "status");
    }

    #[test]
    fn counts_keep_first_appearance_for_ties() {
        let mut c = Counts::default();
        for k in ["b", "a", "c", "a", "c"] {
            c.add(k);
        }
        assert_eq!(c.sorted_desc(), vec![("a", 2), ("c", 2), ("b", 1)]);
        assert_eq!(py_dict_repr(&c), "{'b': 1, 'a': 2, 'c': 2}");
    }

    #[test]
    fn status_json_matches_python_separators() {
        let job = ZoneJob { state: Mutex::new(("compiling".into(), "bounding spheres".into(), None)), zone: OnceLock::new(), aux: OnceLock::new() };
        assert_eq!(String::from_utf8(job.status_json()).unwrap(), r#"{"state": "compiling", "phase": "bounding spheres", "error": null}"#);
    }
}
