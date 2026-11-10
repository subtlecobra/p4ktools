//! Instances of `.socpak` object containers: every entity with a geometry reference, placed in container space.
//!
//! Each distinct geometry file is built once and drawn once per entity through a per-geometry list of 3x4 matrices.  Nested containers
//! (entities of class ObjectContainer plus the children listed in the metadata XML) are flattened up to `MAX_CONTAINER_DEPTH`.  See
//! `p4k_formats::socpak` for what is and is not read from a container.
//!
//! `build_socpak_scene` (the page/export scene of a container: every distinct geometry built once through one material registry) is
//! at the end of this file; the zone compile (`zone.rs`) uses `collect_instances` only.
//!
//! The interaction `catalog` hooks (`tag`, `ladders`) and the transit lists (`p4k_formats::transit`) are wired in by
//! `collect_instances_cat`; `collect_instances` is the same without a catalog.  Containers that fail to open are recorded in
//! `Stats::errors` and skipped instead of failing the whole build.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use p4k_formats::socpak::{self, compose, Entity, SocZip, IDENT};
use p4k::{Entry, P4k};
use serde_json::{json, Value};

use crate::builder::{build_scene_named, GeoSource, Options};
use crate::encode::encode_parts;
use crate::interact::{Catalog, Desc};
use crate::items::ItemMaterialSource;
use crate::materials::{rnd, MatRegistry};
use crate::overlay::Overlay;

/// default for one page; callers may pass more
pub const MAX_INSTANCES: usize = 30000;
/// cycles are detected separately, so this only stops runaway data
pub const MAX_CONTAINER_DEPTH: usize = 32;

/// Archive-relative lowercase path; container references sometimes already start with data/.
pub fn data_path(p: &str) -> String {
    if p.starts_with("data/") {
        p.to_string()
    } else {
        format!("data/{}", p)
    }
}

/// An entity hung on a helper node of another entity : `uid = (scope, guid)`, `parent = (scope, parent guid)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ride {
    pub uid: (usize, String),
    pub parent: (usize, String),
    pub helper: String,
}

/// The interaction tag of an instance (`cls`, `name`, `d`, `scope`, `guid`, `links`; a loadout item has `uid` / `parent`
/// instead of `scope` / `guid`).  Only classes `Catalog::describe` accepts (and that are not ladders) carry one.
#[derive(Debug, Clone)]
pub struct Tag {
    pub cls: String,
    pub name: Option<String>,
    pub d: Arc<Desc>,
    pub scope: Option<usize>,
    pub guid: Option<String>,
    pub links: Vec<String>,
    pub uid: Option<i64>,
    pub parent: Option<i64>,
}

/// A climbable ladder entity (`Stats::ladders`).
#[derive(Debug, Clone)]
pub struct LadderRec {
    pub cls: String,
    pub name: Option<String>,
    pub m: [f64; 12],
    pub params: Value,
    pub geo: Option<String>,
    pub d: Arc<Desc>,
}

/// A transit manager / destination entity (`stats["transit"]` tuple): (description, world matrix, scope, name, guid).
#[derive(Debug, Clone)]
pub struct TransitRec {
    pub info: Value,
    pub m: [f64; 12],
    pub scope: usize,
    pub name: Option<String>,
    pub guid: Option<String>,
}

/// One placed geometry.  `geometry` is the archive entry name, or `<owner socpak entry>#<member, lowercase>` for designer brush geometry
/// that lives in an `Overlay`.
#[derive(Debug, Clone)]
pub struct Instance {
    pub geometry: String,
    pub matrix: [f64; 12],
    /// per-node material override of a render node
    pub mtl: Option<String>,
    /// per-node tint palette path
    pub pal: Option<String>,
    pub ride: Option<Ride>,
    /// the interaction tag (doors, seats, turrets, panels), only with a catalog
    pub tag: Option<Tag>,
}

#[derive(Default)]
pub struct Stats {
    pub errors: Vec<String>,
    pub missing_containers: usize,
    pub skipped_containers: usize,
    pub no_geometry: usize,
    pub missing_geometry: BTreeSet<String>,
    pub truncated: bool,
    pub notes: Vec<String>,
    pub max: usize,
    /// designer brush geometry found inside containers
    pub overlay: Overlay,
    ent_cache: HashMap<(String, Option<String>), Arc<Vec<Entity>>>,
    pub scopes: usize,
    /// (scope, guid) -> GUIDs it links to; GUIDs repeat when one container is instanced twice, so links are per instance
    pub links: HashMap<(usize, String), Vec<String>>,
    /// (class, [x, y, z]) of TransitGateway / TransitInteractionUIPanel* entities
    pub transit_ents: Vec<(String, [f64; 3])>,
    /// matrices of ElevatorDoor* entities that have a model
    pub transit_doors: Vec<[f64; 12]>,
    /// TransitManager / TransitDestination entities (the elevator planner, `elevator.rs`)
    pub transit: Vec<TransitRec>,
    /// climbable ladders (only with a catalog)
    pub ladders: Vec<LadderRec>,
    pub palettes: HashMap<String, usize>,
    /// palette paths in order of first appearance (a stable sort by descending count keeps it for ties)
    pub palette_order: Vec<String>,
    pub lights: Vec<(Value, String, [f64; 12])>,
    pub volumes: Vec<(Value, [f64; 12])>,
    pub decals: Vec<(String, [f64; 12])>,
    pub no_geometry_cls: HashMap<String, usize>,
    /// classes in order of first appearance (see `palette_order`)
    pub no_geometry_order: Vec<String>,
    pub class_geometry: usize,
    /// geometry entry name -> (entity class, geometry path) of the first entity that placed it
    pub geo_cls: HashMap<String, (String, String)>,
}

impl Stats {
    /// The parsed entities of a container read earlier by `collect_instances`, keyed by (name, only_soc).
    pub fn cached_entities(&self, name: &str, only_soc: Option<&str>) -> Option<Arc<Vec<Entity>>> {
        self.ent_cache.get(&(name.to_string(), only_soc.map(str::to_owned))).cloned()
    }

    /// Empty statistics with the instance limit `max` (`usize::MAX` = none); `Stats` has private fields, so build it here.
    pub fn with_max(max: usize) -> Stats {
        Stats { max, ..Default::default() }
    }
}

/// Collapse repeated render-node notes (same text across many containers) into 'text (xN)' lines.
pub fn summarise(notes: &[String]) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for n in notes {
        *counts.entry(n).or_insert(0) += 1;
    }
    counts.into_iter().map(|(n, c)| if c > 1 { format!("{} (x{})", n, c) } else { n.to_string() }).collect()
}

/// Note for entities without a geometry path, naming the commonest classes (EMPIRICAL: in asd_layout_facility_001 about half are logic
/// volumes, markers, lights, particles and audio, which have no mesh).
pub fn no_geometry_note(stats: &Stats, top: usize) -> String {
    // sorted by descending count: a stable sort, ties keep the order of first appearance
    let mut cls: Vec<(&String, &usize)> = stats.no_geometry_order.iter().map(|k| (k, &stats.no_geometry_cls[k])).collect();
    cls.sort_by(|a, b| b.1.cmp(a.1));
    format!(
        "{} entities have no model (logic, audio, comms, flowgraph ... entities: nothing to draw; shown as point markers under Volumes, off by default); most common: {}",
        stats.no_geometry,
        cls.iter().take(top).map(|(k, v)| format!("{} {}", k, v)).collect::<Vec<_>>().join(", ")
    )
}

/// EMPIRICAL: the 3 missing files of asd_layout_facility_001 exist in no case, extension or folder of the archive (stale references to
/// removed variants), so they are reported with their names rather than fixed.
pub fn missing_geometry_note(stats: &Stats, show: usize) -> String {
    let mut names: Vec<&String> = stats.missing_geometry.iter().collect();
    names.sort_by_key(|n| n.rsplit('/').next().unwrap().to_string());
    format!(
        "{} referenced geometry files are not in the archive: {}{}",
        names.len(),
        names.iter().take(show).map(|n| n.rsplit('/').next().unwrap()).collect::<Vec<_>>().join(", "),
        if names.len() > show { ", ..." } else { "" }
    )
}

/// The geometry file of a designer brush, which is stored inside the .socpak that owns it and referenced as
/// '<container dir>/<container name>//brush/x.cgf'.
///
/// EMPIRICAL: 153 such paths in asd_layout_facility_001, each present in the zip as '<container name>/brush/x.cgf'.  Registers the cgf
/// and its same-stem companion files in the overlay and returns the cgf's virtual name, or None when the path is not of that form or the
/// member is absent.
pub fn zip_geometry(p4k: &P4k, path: &str, stats: &mut Stats) -> Option<String> {
    let (prefix, rest) = path.split_once("//")?;
    let owner = p4k.find_path(&data_path(&format!("{}.socpak", prefix)), ".socpak")?;
    let member = format!("{}/{}", prefix.rsplit('/').next().unwrap(), rest);
    let key = format!("{}#{}", owner.name, member.to_lowercase());
    if stats.overlay.contains(&key) {
        return Some(key);
    }
    let data = p4k.read(owner).ok()?;
    let z = SocZip::open(&data).ok()?;
    let stem_of = |n: &str| n.rsplit_once('.').map_or(n.to_string(), |x| x.0.to_string());
    let stem = stem_of(&member.to_lowercase());
    let found: Vec<&str> = z.namelist().filter(|n| stem_of(&n.to_lowercase()) == stem).collect();
    let cgf = found.iter().find(|n| {
        let l = n.to_lowercase();
        l.ends_with(".cgf") || l.ends_with(".cga")
    })?;
    for n in &found {
        let name = format!("{}#{}", owner.name, n.to_lowercase());
        if let Ok(bytes) = z.read(n) {
            stats.overlay.insert(name, bytes);
        }
    }
    let name = format!("{}#{}", owner.name, cgf.to_lowercase());
    stats.overlay.contains(&name).then_some(name)
}

/// Flatten a container (and nested ones) into `out`.  `world` is the container's matrix; `seen` holds the ancestors' entry names.
#[allow(clippy::too_many_arguments)]
pub fn collect_instances(
    p4k: &P4k,
    entry: &Entry,
    world: &[f64; 12],
    depth: usize,
    out: &mut Vec<Instance>,
    stats: &mut Stats,
    seen: &HashSet<String>,
    only_soc: Option<&str>,
    items: Option<&dyn ItemMaterialSource>,
) {
    collect_instances_cat(p4k, entry, world, depth, out, stats, seen, only_soc, items, None)
}

/// `collect_instances` with the interaction catalog (the zone passes one only when the catalog is ready):
/// instances of door / seat / turret / panel classes carry a `Tag`, ladder entities are listed in `Stats::ladders`, and transit entities
/// in `Stats::transit` (always, with or without a catalog).
#[allow(clippy::too_many_arguments)]
pub fn collect_instances_cat(
    p4k: &P4k,
    entry: &Entry,
    world: &[f64; 12],
    depth: usize,
    out: &mut Vec<Instance>,
    stats: &mut Stats,
    seen: &HashSet<String>,
    only_soc: Option<&str>,
    items: Option<&dyn ItemMaterialSource>,
    cat: Option<&Catalog>,
) {
    // The same container is instanced many times (997 reads of 145 containers in asd_layout_facility_001) and parsing its binary XML
    // dominates a zone compile, so parse each (container, .soc filter) once.  Entities are only read afterwards.
    let ckey = (entry.name.to_string(), only_soc.map(str::to_owned));
    if !stats.ent_cache.contains_key(&ckey) {
        let mut notes = vec![];
        let r = p4k.read(entry).map_err(|e| e.to_string()).and_then(|d| socpak::read_entities(&d, Some(&mut notes), only_soc).map_err(|e| e.to_string()));
        let short = entry.name.rsplit('/').next().unwrap();
        let ents = match r {
            Ok((ents, errs)) => {
                stats.errors.extend(errs.iter().take(3).map(|e| format!("{}: {}", short, e)));
                ents
            }
            Err(e) => {
                stats.errors.push(format!("{}: {}", short, e));
                vec![]
            }
        };
        stats.notes.extend(notes);
        stats.ent_cache.insert(ckey.clone(), Arc::new(ents));
    }
    let ents = stats.ent_cache[&ckey].clone();
    stats.scopes += 1;
    let scope = stats.scopes;
    for en in ents.iter() {
        let m = compose(world, &en.matrix);
        let guid = en.guid.as_deref().filter(|g| !g.is_empty());
        let cls = en.cls.as_deref().filter(|c| !c.is_empty());
        if let (false, Some(g)) = (en.links.is_empty(), guid) {
            stats.links.insert((scope, g.to_string()), en.links.clone());
        }
        if let Some(info) = &en.transit {
            stats.transit.push(TransitRec { info: info.clone(), m, scope, name: Some(en.name.clone()), guid: en.guid.clone() });
        }
        if let Some(c) = cls {
            if c == "TransitGateway" || c.starts_with("TransitInteractionUIPanel") {
                stats.transit_ents.push((c.to_string(), [m[3], m[7], m[11]]));
            }
            if c.starts_with("ElevatorDoor") && en.geometry.as_deref().map_or(false, |g| !g.is_empty()) {
                stats.transit_doors.push(m);
            }
        }
        if let Some(container) = en.container.as_deref().filter(|c| !c.is_empty()) {
            match p4k.find_path(&data_path(container), ".socpak") {
                None => stats.missing_containers += 1,
                Some(child) => {
                    if depth >= MAX_CONTAINER_DEPTH || seen.contains(child.name.as_ref()) {
                        stats.skipped_containers += 1;
                    } else {
                        let mut seen2 = seen.clone();
                        seen2.insert(entry.name.to_string());
                        collect_instances_cat(p4k, child, &m, depth + 1, out, stats, &seen2, None, items, cat);
                    }
                }
            }
            continue;
        }
        if !en.palettes.is_empty() {
            for p in &en.palettes {
                let n = stats.palettes.entry(p.clone()).or_insert(0);
                if *n == 0 {
                    stats.palette_order.push(p.clone());
                }
                *n += 1;
            }
            continue;
        }
        if let Some(l) = &en.light {
            stats.lights.push((l.clone(), en.name.clone(), m));
            continue;
        }
        if let Some(v) = &en.volume {
            stats.volumes.push((v.clone(), m));
            continue;
        }
        if let Some(mat) = &en.decal {
            stats.decals.push((mat.clone(), m));
            continue;
        }
        if let (Some(lp), Some(cat), Some(c)) = (&en.ladder, cat, cls) {
            // a climbable ladder (its model, if any, is drawn like any other entity)
            let d = cat.describe(c).unwrap_or_else(|| Arc::new(Desc { kind: "ladder", height: 8.0, back: true, ..Default::default() }));
            stats.ladders.push(LadderRec { cls: c.to_string(), name: Some(en.name.clone()), m, params: lp.clone(), geo: en.geometry.clone(), d });
        }
        let mut geometry = en.geometry.clone().filter(|g| !g.is_empty());
        if geometry.is_none() {
            if let (Some(c), Some(it)) = (cls, items) {
                // the model of an item / prop entity lives in its DataCore class record
                if let Some(g) = it.class_geometry(c).filter(|g| !g.is_empty()) {
                    geometry = Some(g.replace('\\', "/").to_lowercase());
                    stats.class_geometry += 1;
                }
            }
        }
        let Some(geometry) = geometry else {
            stats.no_geometry += 1;
            let c = cls.unwrap_or("(no class)").to_string();
            let n = stats.no_geometry_cls.entry(c.clone()).or_insert(0);
            if *n == 0 {
                stats.no_geometry_order.push(c);
            }
            *n += 1;
            if let Some(c) = cls {
                // shown as a point marker named after its class (volumes feed, off by default)
                stats.volumes.push((json!({"kind": "marker", "cls": c, "members": []}), m));
            }
            continue;
        };
        let gpath = data_path(&geometry);
        let flat = gpath.replace("//", "/");
        let found: Option<String> = p4k
            .find_path(&flat, ".cgf")
            .or_else(|| p4k.find_path(&flat, ".cga"))
            .map(|e| e.name.to_string())
            .or_else(|| zip_geometry(p4k, &gpath, stats));
        let Some(gname) = found else {
            stats.missing_geometry.insert(geometry);
            continue;
        };
        if out.len() >= stats.max {
            stats.truncated = true;
            return;
        }
        if let Some(c) = cls {
            // the entity class of a model: the zone reads the authored wear of its record (`itemwear`); the first class wins
            stats.geo_cls.entry(gname.clone()).or_insert((c.to_string(), gpath.clone()));
        }
        let mut tag = None;
        if let (Some(cat), Some(c)) = (cat, cls) {
            // doors / seats / turrets: the zone compiler splits and indexes them (`interact`)
            if let Some(d) = cat.describe(c).filter(|d| d.kind != "ladder") {
                tag = Some(Tag { cls: c.to_string(), name: Some(en.name.clone()), d, scope: Some(scope), guid: en.guid.clone(), links: en.links.clone(), uid: None, parent: None });
            }
        }
        let ride = match (&en.attach, guid) {
            (Some(att), Some(g)) => Some(Ride { uid: (scope, g.to_lowercase()), parent: (scope, att.parent.clone()), helper: att.target.clone() }),
            _ => None,
        };
        out.push(Instance { geometry: gname, matrix: m, mtl: en.mtl.clone(), pal: en.pal.clone(), ride, tag });
    }
}

/// Everything `collect_instances` produced for one container.
pub struct Collected {
    pub instances: Vec<Instance>,
    pub stats: Stats,
}

/// Flatten `entry` (and its nested containers) with the page default of `max_instances`.  `only_soc`: show just that .soc file of the
/// container (see `socpak::read_entities`).
pub fn collect(p4k: &P4k, entry: &Entry, only_soc: Option<&str>, max_instances: usize, items: Option<&dyn ItemMaterialSource>) -> Collected {
    let mut out = vec![];
    let mut stats = Stats { max: max_instances, ..Default::default() };
    collect_instances(p4k, entry, &IDENT, 0, &mut out, &mut stats, &HashSet::new(), only_soc, items);
    Collected { instances: out, stats }
}

/// Matrices per geometry, geometries in name order (the first half of `build_socpak_scene`).
pub fn group_by_geometry(instances: &[Instance]) -> BTreeMap<&str, Vec<[f64; 12]>> {
    let mut by: BTreeMap<&str, Vec<[f64; 12]>> = BTreeMap::new();
    for i in instances {
        by.entry(i.geometry.as_str()).or_default().push(i.matrix);
    }
    by
}

/// Knobs of [`build_socpak_scene`] (and their defaults).
#[derive(Debug, Clone)]
pub struct SceneOptions {
    pub with_render: bool,
    pub with_collision: bool,
    pub max_render_tris: usize,
    pub max_tex: usize,
    pub with_textures: bool,
    pub only_soc: Option<String>,
    pub max_instances: usize,
}

impl Default for SceneOptions {
    fn default() -> Self {
        SceneOptions { with_render: true, with_collision: true, max_render_tris: 60000, max_tex: 128, with_textures: true, only_soc: None, max_instances: MAX_INSTANCES }
    }
}

/// The scene JSON of a container for the page / export: `{info, bbox, parts, geoms: [{name, mats}], materials, textures}`.  Each distinct
/// geometry file is built once (`builder::build_scene_named`, through one `MatRegistry`) and drawn once per entity by the viewer through
/// its list of 3x4 matrices.  A geometry that fails to build is reported in `info.errors` and skipped (one unreadable geometry must not
/// blank the scene); the error text is the Rust error's.
pub fn build_socpak_scene(p4k: &P4k, entry: &Entry, o: &SceneOptions, items: Option<&dyn ItemMaterialSource>) -> Value {
    let Collected { instances: out, stats } = collect(p4k, entry, o.only_soc.as_deref(), o.max_instances, None); // stats carry no items here, only the registry
    let by_geom = group_by_geometry(&out);
    let mut registry = MatRegistry::with_options(p4k, o.max_tex, o.with_textures, false);
    registry.items = items;
    let src = GeoSource::with_overlay(p4k, &stats.overlay);
    let tail = |n: &str| n.rsplit('/').next().unwrap().to_string();
    let (mut parts, mut geoms, mut errors): (Vec<Value>, Vec<Value>, Vec<String>) = (vec![], vec![], stats.errors.clone());
    let (mut counts, mut render_tris): (BTreeMap<String, usize>, usize) = (BTreeMap::new(), 0);
    let (mut mn, mut mx) = ([1e30f64; 3], [-1e30f64; 3]);
    let opts = Options { with_render: o.with_render, with_collision: o.with_collision, max_render_tris: Some(o.max_render_tris), ..Options::default() };
    for (gname, mats) in &by_geom {
        let (info, gparts) = match build_scene_named(&src, gname, &opts, &mut registry) {
            Ok(x) => x,
            Err(ex) => {
                errors.push(format!("{}: {}", tail(gname), ex));
                continue;
            }
        };
        let (mut enc, bbox) = encode_parts(&gparts, false);
        let gi = geoms.len();
        for pt in enc.iter_mut() {
            pt["g"] = json!(gi);
        }
        parts.extend(enc);
        let n = mats.len();
        for (k, v) in &info.collision_counts {
            *counts.entry(k.clone()).or_insert(0) += v * n;
        }
        render_tris += info.render_tris.unwrap_or(0) * n;
        errors.extend(info.errors.iter().take(2).map(|e| format!("{}: {}", tail(gname), e)));
        let f = |v: &Value| v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect::<Vec<_>>();
        let (lo, hi) = (f(&bbox[0]), f(&bbox[1]));
        for m in mats {
            for ci in 0..8 {
                let c = [if ci & 1 == 0 { lo[0] } else { hi[0] }, if ci & 2 == 0 { lo[1] } else { hi[1] }, if ci & 4 == 0 { lo[2] } else { hi[2] }];
                let w = p4k_formats::ivo::xform(m, c);
                for k in 0..3 {
                    mn[k] = mn[k].min(w[k]);
                    mx[k] = mx[k].max(w[k]);
                }
            }
        }
        geoms.push(json!({"name": gname, "mats": mats.iter().map(|m| m.iter().map(|v| rnd(*v, 6)).collect::<Vec<_>>()).collect::<Vec<_>>()}));
    }
    let bbox = if mn[0] > mx[0] { json!([[-1, -1, -1], [1, 1, 1]]) } else { json!([mn, mx]) };
    let mut notes = vec![
        format!("{} entities with geometry, {} distinct geometry files (each built once, instanced)", out.len(), by_geom.len()),
        format!("render meshes capped at {} tris per geometry, textures at {} px", o.max_render_tris, o.max_tex),
        "entity rotation read as w,x,y,z; Scale applied per axis; per-entity material overrides ignored".to_string(),
    ];
    if let Some(s) = &o.only_soc {
        notes.insert(0, format!("showing only {} of this container (no .entxml entities, no nested containers)", s));
    }
    if stats.no_geometry > 0 {
        notes.push(no_geometry_note(&stats, 6));
    }
    if stats.missing_containers > 0 || stats.skipped_containers > 0 {
        notes.push(format!(
            "nested containers: {} not found in the archive, {} skipped (depth {} / cycle)",
            stats.missing_containers, stats.skipped_containers, MAX_CONTAINER_DEPTH
        ));
    }
    if !stats.missing_geometry.is_empty() {
        notes.push(missing_geometry_note(&stats, 3));
    }
    if stats.truncated {
        notes.push(format!("truncated at {} instances (pass ?max=N to the URL for more)", stats.max));
    }
    notes.extend(summarise(&stats.notes));
    notes.extend(registry.notes.iter().cloned());
    let info = json!({"asset": entry.name.as_ref(), "companion": null, "nodes": out.len(), "notes": notes, "errors": errors.iter().take(40).collect::<Vec<_>>(),
        "render_tris": render_tris, "collision_counts": counts, "instances": out.len()});
    let (materials, textures) = registry.export();
    json!({"info": info, "bbox": bbox, "parts": parts, "geoms": geoms, "materials": materials, "textures": textures})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_prefix() {
        assert_eq!(data_path("objects/a.cgf"), "data/objects/a.cgf");
        assert_eq!(data_path("data/objects/a.cgf"), "data/objects/a.cgf");
    }

    #[test]
    fn notes_summarise() {
        let n = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(summarise(&n), ["a", "b (x2)"]);
    }

    #[test]
    fn groups_sorted() {
        let mk = |g: &str| Instance { geometry: g.into(), matrix: IDENT, mtl: None, pal: None, ride: None, tag: None };
        let v = vec![mk("b"), mk("a"), mk("b")];
        let g = group_by_geometry(&v);
        assert_eq!(g.keys().copied().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(g["b"].len(), 2);
    }
}
