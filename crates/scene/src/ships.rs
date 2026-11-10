//! Ships as the game assembles them: a hull model plus several object containers, each placed by an offset.
//!
//! A ship's DataCore record (`EntityClassDefinition.<class>`) has a `VehicleComponentParams` component whose `objectContainers` lists
//! `SVehicleObjectContainerParams` {fileName, Offset{Position, Rotation (Ang3)}, boneName}, and an `SGeometryResourceParams` component with
//! the exterior hull (.cga).  EMPIRICAL on the live archive: 1,147 records list containers (4,964 entries, 696 distinct files).  The offsets
//! are used as placements in ship space (INFERRED: the Idris sections come out nose to tail in y order); a container's bone-relative meaning
//! is not evaluated; Ang3 is degrees applied as Rz * Ry * Rx (INFERRED; the only non-zero angles in the archive are about Z: 180, +-45).
//!
//! The default loadout (`loadout_tree`) places each item on the helper node its port definition names (EMPIRICAL, AEGS_Javelin: 145 of the
//! 156 top-level port names are node names of the hull; 452 port definitions have an implementation of type ...Bone with a helper name and
//! an offset, and for 309 of them the helper is NOT the port name; relay hardpoints are ...Entity implementations carrying the same data).
//! `swaps` ({port path: class}) replace the item of a port; `accept` filters the children kept under a swapped item (`swaps`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use p4k_formats::dcb::Value as Dv;
use p4k_formats::ivo;
use p4k_formats::socpak::{compose, quat_scale_pos_matrix, IDENT};
use p4k::P4k;
use serde_json::{json, Value};

use crate::bed::round_n;
use crate::itemmaterials::ItemMaterials;
use crate::paints;
use crate::station;
use crate::zone::{Extra, Livery, ZoneSpec};

pub const RECORD: &str = "EntityClassDefinition.";
pub const LOADOUT_DEPTH: usize = 4;
pub type Mat = [f64; 12];

fn quat_mul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    let (aw, ax, ay, az) = (a[0], a[1], a[2], a[3]);
    let (bw, bx, by, bz) = (b[0], b[1], b[2], b[3]);
    [aw * bw - ax * bx - ay * by - az * bz, aw * bx + ax * bw + ay * bz - az * by, aw * by - ax * bz + ay * bw + az * bx, aw * bz + ax * by - ay * bx + az * bw]
}

/// 3x4 row-major matrix of an Offset: position and Ang3 rotation in degrees (Rz * Ry * Rx).
pub fn offset_matrix(pos: [f64; 3], rot_deg: [f64; 3]) -> Mat {
    let h: Vec<f64> = rot_deg.iter().map(|a| a.to_radians() / 2.0).collect();
    let qx = [h[0].cos(), h[0].sin(), 0.0, 0.0];
    let qy = [h[1].cos(), 0.0, h[1].sin(), 0.0];
    let qz = [h[2].cos(), 0.0, 0.0, h[2].sin()];
    quat_scale_pos_matrix(quat_mul(qz, quat_mul(qy, qx)), [1.0; 3], pos)
}

fn vec3(v: Option<&Dv>) -> [f64; 3] {
    let g = |k: &str| v.and_then(|x| x.get(k)).and_then(Dv::as_f64).unwrap_or(0.0);
    [g("x"), g("y"), g("z")]
}

/// One `objectContainers` entry of a ship record.
#[derive(Debug, Clone)]
pub struct ContainerRef {
    pub file: String,
    pub bone: Option<String>,
    pub pos: [f64; 3],
    pub rot: [f64; 3],
}

#[derive(Debug, Clone)]
pub struct ShipRecord {
    pub geometry: Option<String>,
    pub containers: Vec<ContainerRef>,
}

/// A node of the loadout tree.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub port: String,
    pub cls: String,
    pub children: Vec<TreeNode>,
    pub geo: Option<String>,
    /// 12 floats rounded to 4 decimals, only when the node has a model placed on its parent
    pub m: Option<Mat>,
    pub helper: Option<String>,
    pub sel: &'static str,
}

/// A placed loadout item (an entry of `Ships::loadout_items`).
#[derive(Debug, Clone)]
pub struct PlacedItem {
    pub geo: String,
    pub matrix: Mat,
    pub cls: String,
    pub port: String,
    pub uid: usize,
    pub parent: Option<usize>,
    pub helper: Option<String>,
}

type PortHelpers = HashMap<String, (String, Option<Mat>)>;

/// Ship records and loadouts over one `ItemMaterials` (with their caches).
pub struct Ships<'a> {
    pub items: &'a ItemMaterials,
    pub p4k: &'a P4k,
    helpers: Mutex<HashMap<String, Arc<PortHelpers>>>,
    nodes: Mutex<HashMap<String, Arc<HashMap<String, Mat>>>>,
    own: Mutex<HashMap<String, Arc<Vec<Dv>>>>,
}

fn field<'v>(v: &'v Dv, k: &str) -> Option<&'v Dv> {
    v.get(k)
}

fn list<'v>(v: Option<&'v Dv>) -> &'v [Dv] {
    match v {
        Some(Dv::List(l)) => l.as_slice(),
        _ => &[],
    }
}

fn opt_str(v: Option<&Dv>) -> Option<String> {
    v.and_then(Dv::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Archive lookup key of a path written in a record ('objectcontainers\\ships\\...' or 'ObjectContainers/...').
pub fn data_key(path: &str) -> String {
    let p = path.replace('\\', "/").to_lowercase();
    let p = p.trim_start_matches('/');
    if p.starts_with("data/") {
        p.to_string()
    } else {
        format!("data/{}", p)
    }
}

impl<'a> Ships<'a> {
    pub fn new(items: &'a ItemMaterials, p4k: &'a P4k) -> Ships<'a> {
        Ships { items, p4k, helpers: Mutex::new(HashMap::new()), nodes: Mutex::new(HashMap::new()), own: Mutex::new(HashMap::new()) }
    }

    pub(crate) fn data(&self, cls: &str, depth: usize) -> Option<Dv> {
        let d = self.items.datacore()?;
        let ids = d.record_index().get(&format!("{}{}", RECORD, cls))?;
        d.record_data(&d.record(*ids.first()?).ok()?, depth).ok()
    }

    /// {geometry, containers} of a ship class, or None when the class has no vehicle component.
    pub fn ship_record(&self, cls: &str) -> Option<ShipRecord> {
        let data = self.data(cls, 8)?;
        for comp in list(data.get("Components")) {
            if matches!(comp, Dv::Obj(o) if o.struct_name == "VehicleComponentParams") {
                let mut containers = vec![];
                for o in list(comp.get("objectContainers")) {
                    let off = o.get("Offset");
                    containers.push(ContainerRef {
                        file: o.get("fileName").and_then(Dv::as_str).unwrap_or("").to_string(),
                        bone: opt_str(o.get("boneName")),
                        pos: vec3(off.and_then(|x| x.get("Position"))),
                        rot: vec3(off.and_then(|x| x.get("Rotation"))),
                    });
                }
                return Some(ShipRecord { geometry: self.items.class_geometry(cls), containers });
            }
        }
        None
    }

    /// The `ZoneSpec` of `/zonedata/ship:<cls>`: the containers placed by their record offsets, the hull model,
    /// the default loadout items with their riders and palettes, the livery of `paint` ('' = the default) and the vehicle commands.
    pub fn ship_zone_spec(&self, cls: &str, paint: Option<&str>) -> Result<ZoneSpec, String> {
        let rec = self.ship_record(cls).ok_or_else(|| format!("no vehicle component: {}", cls))?;
        let cidx = self.p4k.lower_index(".socpak");
        let (mut parts, mut missing) = (vec![], vec![]);
        for c in &rec.containers {
            match cidx.map.get(&data_key(&c.file)) {
                Some(&i) => parts.push((self.p4k.entries[i].name.to_string(), offset_matrix(c.pos, c.rot))),
                None => missing.push(c.file.replace('\\', "/").rsplit('/').next().unwrap().to_string()),
            }
        }
        let mut unplaced = 0usize;
        let extras: Vec<Extra> = self
            .loadout_items(cls, &mut unplaced, None, None)
            .into_iter()
            .map(|it| Extra {
                pal: paints::class_palette(self.items, &it.cls),
                path: it.geo,
                matrix: it.matrix,
                cls: Some(it.cls),
                port: Some(it.port),
                uid: Some(it.uid as i64),
                parent: it.parent.map(|p| p as i64),
                helper: it.helper,
            })
            .collect();
        let entries = self.loadout_entries(cls);
        let livery = paints::resolve(self.items, cls, paint, entries.as_deref()).map(|l| Livery { id: Some(l.id), palette: l.palette, material: l.material });
        let hull_name = rec.geometry.as_deref().map(|g| g.replace('\\', "/")).map(|g| g.rsplit('/').next().unwrap().to_string()).unwrap_or_else(|| "none".into());
        let mut notes = vec![
            format!("default loadout: {} items placed at the hull's helper nodes, {} ports without a node", extras.len(), unplaced),
            format!("ship {}: {} containers placed by their record offsets (INFERRED as ship-space placements), hull {}", cls, parts.len(), hull_name),
        ];
        if !missing.is_empty() {
            let mut u: Vec<String> = missing.clone();
            u.sort();
            u.dedup();
            notes.push(format!("{} containers of the record are not in the archive: {}", missing.len(), u.iter().take(6).cloned().collect::<Vec<_>>().join(", ")));
        }
        Ok(ZoneSpec {
            parts: Some(parts),
            title: Some(format!("Ship {}", cls)),
            hull: rec.geometry.clone(),
            extras,
            livery,
            extra_notes: notes,
            ship_cmds: self.ship_commands(cls),
            ..Default::default()
        })
    }

    /// The default-loadout entries of a ship record (`None` when the class is unknown).
    pub fn loadout_entries(&self, cls: &str) -> Option<Vec<Dv>> {
        let data = self.data(cls, 12)?;
        let mut entries = vec![];
        for comp in list(data.get("Components")) {
            if matches!(comp, Dv::Obj(o) if o.struct_name == "SEntityComponentDefaultLoadoutParams") {
                entries = list(comp.get("loadout").and_then(|l| l.get("entries"))).to_vec();
            }
        }
        Some(entries)
    }

    /// {item port name (lower case): (helper node name, 3x4 offset or None)} from the port definitions of an entity class
    /// (`SItemPortContainerComponentParams.Ports[].AttachmentImplementation`; EMPIRICAL, see the module docs).
    pub fn port_helpers(&self, cls: &str) -> Arc<PortHelpers> {
        if let Some(v) = self.helpers.lock().unwrap().get(cls) {
            return v.clone();
        }
        let mut out: PortHelpers = HashMap::new();
        if !cls.is_empty() {
            if let Some(data) = self.data(cls, 12) {
                for comp in list(data.get("Components")) {
                    if !matches!(comp, Dv::Obj(o) if o.struct_name == "SItemPortContainerComponentParams") {
                        continue;
                    }
                    for pt in list(comp.get("Ports")) {
                        let Some(ai) = field(pt, "AttachmentImplementation") else { continue };
                        let sname = match ai {
                            Dv::Obj(o) => o.struct_name.as_str(),
                            _ => continue,
                        };
                        if sname != "SItemPortDefAttachmentImplementationBone" && sname != "SItemPortDefAttachmentImplementationEntity" {
                            continue;
                        }
                        let Some(h) = ai.get("Helper").and_then(|x| x.get("Helper")) else { continue };
                        let Some(name) = opt_str(h.get("Name")) else { continue };
                        let off = h.get("Offset");
                        let (pos, rot) = (vec3(off.and_then(|o| o.get("Position"))), vec3(off.and_then(|o| o.get("Rotation"))));
                        let nz = pos.iter().any(|v| v.abs() > 1e-6) || rot.iter().any(|v| v.abs() > 1e-6);
                        out.insert(pt.get("Name").and_then(Dv::as_str).unwrap_or("").to_lowercase(), (name, if nz { Some(offset_matrix(pos, rot)) } else { None }));
                    }
                }
            }
        }
        let a = Arc::new(out);
        self.helpers.lock().unwrap().insert(cls.to_string(), a.clone());
        a
    }

    /// {lower-case node name: world matrix} of a model path as written in a record (cached; unreadable = empty).
    fn nodes_of(&self, path: &str) -> Arc<HashMap<String, Mat>> {
        if let Some(v) = self.nodes.lock().unwrap().get(path) {
            return v.clone();
        }
        let key = data_key(path);
        let mut table = HashMap::new();
        for ext in [".cgf", ".cga"] {
            if key.ends_with(ext) {
                let idx = self.p4k.lower_index(ext);
                if let Some(&i) = idx.map.get(&key) {
                    let e = &self.p4k.entries[i];
                    if let Some(cg) = self.p4k.read(e).ok().and_then(|b| ivo::parse_cgf(&b, false).ok()) {
                        for n in &cg.nodes {
                            table.insert(n.name.to_lowercase(), n.world);
                        }
                    }
                }
            }
        }
        let a = Arc::new(table);
        self.nodes.lock().unwrap().insert(path.to_string(), a.clone());
        a
    }

    fn own_loadout(&self, cls: &str) -> Arc<Vec<Dv>> {
        if let Some(v) = self.own.lock().unwrap().get(cls) {
            return v.clone();
        }
        let a = Arc::new(self.loadout_entries(cls).unwrap_or_default());
        self.own.lock().unwrap().insert(cls.to_string(), a.clone());
        a
    }

    /// The ship's default loadout as a tree in record order (empty ports kept out), with the placement of each item; `unplaced` counts the
    /// ports without a node.  `swaps` = {port path: class}; `accept(child path, child class)` decides which of the old item's children stay.
    pub fn loadout_tree(&self, cls: &str, unplaced: &mut usize, swaps: Option<&HashMap<String, String>>, accept: Option<&dyn Fn(&str, &str) -> bool>) -> Vec<TreeNode> {
        let Some(entries) = self.loadout_entries(cls) else { return vec![] };
        let hull = self.items.class_geometry(cls);
        let hull_nodes = hull.as_deref().map(|h| self.nodes_of(h));
        let mut tree = self.build(&entries, hull_nodes, IDENT, 0, cls, false, "", unplaced, swaps, accept);
        mark_selectable(&mut tree, false);
        tree
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        entries: &[Dv],
        parent_nodes: Option<Arc<HashMap<String, Mat>>>,
        parent_matrix: Mat,
        depth: usize,
        parent_cls: &str,
        inherited: bool,
        path: &str,
        unplaced: &mut usize,
        swaps: Option<&HashMap<String, String>>,
        accept: Option<&dyn Fn(&str, &str) -> bool>,
    ) -> Vec<TreeNode> {
        let mut out = vec![];
        let helpers = self.port_helpers(parent_cls);
        for e in entries {
            let Dv::Obj(eo) = e else { continue };
            let mut ecls = self.items.entry_class(eo);
            let port = eo.get("itemPortName").and_then(Dv::as_str).unwrap_or("").to_lowercase();
            let here = if path.is_empty() { port.clone() } else { format!("{}/{}", path, port) };
            let swapped = swaps.map_or(false, |s| s.get(&here).map_or(false, |c| *c != ecls));
            if swapped {
                ecls = swaps.unwrap()[&here].clone();
            }
            let geo = if !ecls.is_empty() { self.items.class_geometry(&ecls) } else { None };
            let (hn, off) = helpers.get(&port).cloned().unwrap_or((port.clone(), None));
            let mut node = parent_nodes.as_ref().and_then(|n| n.get(&hn.to_lowercase())).copied();
            let mut off = off;
            if node.is_none() && hn.to_lowercase() != port {
                node = parent_nodes.as_ref().and_then(|n| n.get(&port)).copied();
                off = None;
            }
            if node.is_none() {
                if inherited {
                    continue; // an item's own default loadout is only shown where its model has the port node
                }
                *unplaced += 1;
            }
            let m = node.map(|n| {
                let m = compose(&parent_matrix, &n);
                match off {
                    Some(o) => compose(&m, &o),
                    None => m,
                }
            });
            let mut sub: Option<Vec<Dv>> = match eo.get("loadout") {
                Some(l @ Dv::Obj(_)) => {
                    let ent = list(l.get("entries"));
                    Some(ent.to_vec())
                }
                _ => None,
            };
            let mut inh = false;
            if swapped && sub.as_ref().map_or(false, |s| !s.is_empty()) {
                let kept: Vec<Dv> = sub
                    .take()
                    .unwrap()
                    .into_iter()
                    .filter(|x| match x {
                        Dv::Obj(xo) => accept.map_or(true, |f| f(&format!("{}/{}", here, xo.get("itemPortName").and_then(Dv::as_str).unwrap_or("").to_lowercase()), &self.items.entry_class(xo))),
                        _ => false,
                    })
                    .collect();
                sub = Some(kept);
                inh = true;
            }
            if sub.as_ref().map_or(true, Vec::is_empty) && !ecls.is_empty() && m.is_some() && geo.is_some() && depth < LOADOUT_DEPTH {
                // the item's own record carries a default loadout (EMPIRICAL: RELAY_1slot / RELAY_3slot list their fuses); the game fills the ports with it
                sub = Some(self.own_loadout(&ecls).as_ref().clone());
                inh = true;
            }
            let child_nodes = if m.is_some() && geo.is_some() && depth < LOADOUT_DEPTH { Some(self.nodes_of(geo.as_deref().unwrap())) } else { None };
            let kids = self.build(sub.as_deref().unwrap_or(&[]), child_nodes, m.unwrap_or(IDENT), depth + 1, &ecls, inh, &here, unplaced, swaps, accept);
            if !ecls.is_empty() || !kids.is_empty() {
                out.push(TreeNode {
                    port: eo.get("itemPortName").and_then(Dv::as_str).unwrap_or("").to_string(),
                    cls: ecls,
                    children: kids,
                    geo: geo.clone(),
                    m: match (&m, &geo) {
                        (Some(m), Some(_)) => Some(m.map(|v| round_n(v, 4))),
                        _ => None,
                    },
                    helper: if node.is_some() { Some(hn) } else { None },
                    sel: "",
                });
            }
        }
        out
    }

    /// Every placed item of the default loadout (see `loadout_tree`), flattened in record order with uid / parent / helper.
    pub fn loadout_items(&self, cls: &str, unplaced: &mut usize, swaps: Option<&HashMap<String, String>>, accept: Option<&dyn Fn(&str, &str) -> bool>) -> Vec<PlacedItem> {
        let tree = self.loadout_tree(cls, unplaced, swaps, accept);
        let mut out = vec![];
        fn flat(nodes: &[TreeNode], parent: Option<usize>, out: &mut Vec<PlacedItem>) {
            for n in nodes {
                let mut uid = None;
                if let Some(m) = n.m {
                    let u = out.len();
                    uid = Some(u);
                    out.push(PlacedItem { geo: n.geo.clone().unwrap_or_default(), matrix: m, cls: n.cls.clone(), port: n.port.clone(), uid: u, parent, helper: n.helper.clone() });
                }
                flat(&n.children, uid.or(parent), out);
            }
        }
        flat(&tree, None, &mut out);
        out
    }

    fn shared_names(&self, cls: &str) -> Vec<Value> {
        let Some(d) = self.items.datacore() else { return vec![] };
        let Some(ids) = d.record_index().get(&format!("{}{}", RECORD, cls)) else { return vec![] };
        let r: Option<Vec<Value>> = (|| {
            let top = d.record_data(&d.record(ids[0]).ok()?, 0).ok()?;
            for c in list(top.get("Components")) {
                if let Dv::Strong(s, _) = c {
                    if *s != p4k_formats::dcb::NULL_PTR && d.struct_name(*s as usize).ok()? == "SEntityInteractableParams" {
                        let j = d.deref(c.clone(), 0, 7).ok()?.to_json();
                        let inter = j.get("Interactable").cloned().unwrap_or(json!({}));
                        return Some(
                            inter
                                .get("SharedInteractions")
                                .and_then(Value::as_array)
                                .map(Vec::as_slice)
                                .unwrap_or(&[])
                                .iter()
                                .filter(|x| x.is_object() && station::truthy(x.get("Name")))
                                .map(|x| x["Name"].clone())
                                .collect(),
                        );
                    }
                }
            }
            Some(vec![])
        })();
        r.unwrap_or_default()
    }

    /// Vehicle commands of a ship (station::vehicle_commands) from its own SharedInteractions and those of the Controller_* entities of
    /// its default loadout.  EMPIRICAL (Idris, Javelin, Redeemer): the seats list no commands; the ship's controllers do.
    pub fn ship_commands(&self, cls: &str) -> Vec<Value> {
        if self.items.datacore().is_none() {
            return vec![];
        }
        let mut names = self.shared_names(cls);
        for e in self.loadout_entries(cls).unwrap_or_default() {
            if let Dv::Obj(eo) = &e {
                let c = self.items.entry_class(eo);
                if c.starts_with("Controller_") {
                    names.extend(self.shared_names(&c));
                }
            }
        }
        station::vehicle_commands(&names)
    }
}

/// Sets `sel` on every node of a placed loadout tree: 'own' = the node has a model placed on the hull; 'kids' = no model of its own but
/// items below it have; 'parent' = unplaced but an ancestor is; 'none' = nothing to show.  Returns True when anything is placed.
pub fn mark_selectable(nodes: &mut [TreeNode], anc: bool) -> bool {
    let mut any = false;
    for n in nodes.iter_mut() {
        let own = n.m.is_some();
        let below = mark_selectable(&mut n.children, anc || own);
        n.sel = if own {
            "own"
        } else if below {
            "kids"
        } else if anc {
            "parent"
        } else {
            "none"
        };
        any = any || own || below;
    }
    any
}

/// The ship sounds (`shipsounds.rs`) ask the ship layer where the default loadout sits; this is `Ships::loadout_items` without swaps,
/// flattened to what they need (class, port, matrix).  Every placed item has a matrix (a node without one is not listed).
impl crate::shipsounds::LoadoutSource for Ships<'_> {
    fn loadout_items(&self, cls: &str) -> Vec<crate::shipsounds::PlacedItem> {
        let mut unplaced = 0usize;
        Ships::loadout_items(self, cls, &mut unplaced, None, None)
            .into_iter()
            .map(|it| crate::shipsounds::PlacedItem { cls: Some(it.cls), port: Some(it.port), matrix: Some(it.matrix) })
            .collect()
    }
}
