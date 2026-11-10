//! Scene assembly for one .cgf / .cga: render meshes (with materials and UVs) plus decoded collision shapes (the
//! evidence notes are kept with the rules they justify).
//!
//! A scene is (info, parts).  Each part is {name, kind, node, verts, tris, [uv], [mat], [nrm], [tan]} in the asset's own
//! coordinates; kind is 'render', 'proxy' (render nodes that look like collision proxies) or a collision shape kind ('box',
//! 'sphere', 'cylinder', 'capsule', 'mesh', 'cluster').
//!
//! The item-record (Game2.dcb) material lookup of `resolve_material_path` is the optional `ItemMaterialSource` of the registry
//! (`MatRegistry::items`); without one that branch is skipped (the command line
//! tools never pass one).  The DataCore-backed source is passed by the callers that have the item records.

use std::collections::{BTreeMap, HashMap, HashSet};

use p4k_formats::ivo::{self, Frame, FrameReader, Node, V3};
use p4k::{Entry, P4k};

use crate::geometry::geom_to_mesh;
use crate::items::ItemMaterialSource;
use crate::materials::MatRegistry;
use crate::overlay::Overlay;

// Collision shapes that do not stop the player.  CONFIRMED against the engine: the player collides with a geometry
// iff its pe_geomparams flags carry geom_colltype_player (0x2).  The node record's phys_flags (+0x98) remove that bit: 0x20 = $IP
// interaction point (flags 0x20000 only: seen by the interaction ray, not solid), 0x10 = $shield hit volume (flags 0x40),
// 0x40 = interior volume (localgrid_ / *no_collide* grid volumes, flags 0x4000).  The props text of a node can say
// "no_player_collide" (clears 0x2) or "player_only_collision" (player-only solid, stays an obstacle here).  EMPIRICAL census
// (~20k files; 3,000 here): $IP_* nodes carry 0x20 / 0x2c, $shield_* 0x1c, localgrid* 0x4c; 0x04 / 0x08 (animated parts)
// do not affect collision.  The names are kept as a fallback for nodes without flags (EMPIRICAL: they always agree).
pub const NODE_IP: u32 = 0x20;
pub const NODE_SHIELD: u32 = 0x10;
pub const NODE_INTERIOR_VOLUME: u32 = 0x40;

/// 'ip' | 'shield' | 'nocollide' for collision geometry the player walks through (see above), else '' (a player obstacle).
pub fn collision_class(node_name: &str, phys_flags: u32, props: &str) -> &'static str {
    let lprops = props.to_lowercase();
    if phys_flags & NODE_IP != 0 {
        return "ip";
    }
    if phys_flags & NODE_SHIELD != 0 {
        return "shield";
    }
    if phys_flags & NODE_INTERIOR_VOLUME != 0 || lprops.contains("no_player_collide") {
        return "nocollide";
    }
    if lprops.contains("player_only_collision") {
        return "";
    }
    let n = node_name.to_lowercase();
    if n.starts_with("$ip_") {
        return "ip";
    }
    if n.starts_with("$shield") {
        return "shield";
    }
    if n.contains("no_collide") {
        return "nocollide";
    }
    ""
}

// Group-0 collision geometry (hull trimesh, box, ...) of a node that ALSO has a group-4 cluster mesh is a coarse companion of the
// detailed cluster, not a walkable shape.  EMPIRICAL (ARGO Mole): the hulls of 'Front' and 'Rear' are closed solids whose cut faces
// (material id 255, 70 + 82 triangles on the plane y=1.69) form a wall across the whole ship cross section, blocking every
// corridor there; the cluster meshes of the same nodes have the real floors and walls.  The same group-0 + group-4 pairing exists
// on 19k+ nodes of the spaceship models.  Engine (CONFIRMED): group-0 geometry gets pe_geomparams flagsCollider 0x81
// (includes geom_colltype_player 0x2) and the cluster mesh flagsCollider 0, so the engine's own rule is NOT confirmed to skip the
// hull; whether the live player capsule collides with either class is OPEN (DataCore 'PhysFlagsColliderOverride').  Skipping the
// hull for walking is INFERRED from the geometry (a cap across a corridor cannot be a walking surface of the real ship).  The
// class is only a walk-mode switch ('hull').
pub const COMPANION_CLASS: &str = "hull";

/// `cls` (a collision_class result), or 'hull' for group-0 geometry on a node that also has a cluster mesh (see above).
pub fn companion_class(cls: &'static str, group: u32, node_has_cluster: bool) -> &'static str {
    if cls.is_empty() && group == ivo::PHYS_GEOM && node_has_cluster {
        COMPANION_CLASS
    } else {
        cls
    }
}

// Collision faces whose material's surface type does not collide with the player (formats/surfacetypes: shield_* surfaces have
// coll_types "-float,shield" = shield class only, CONFIRMED against the engine; mat_no_collide has no_collide).  The
// per-triangle byte of a trimesh / cluster mesh is the sub-material index of the cgf's material (EMPIRICAL: on the hangar shield
// models the shield faces carry the indices of the 'shield' / 'shield_static' sub-materials, the frames those of metal ones; values
// outside the material are left alone).  That the physics filters per triangle by that surface is INFERRED (consumer not located).
// The faces get the walk class 'nonplayer' (off for walking, drawn in their own colour in the collision views).
pub const SURFACE_CLASS: &str = "nonplayer";

/// One scene part.  Optional vectors are empty when absent.
#[derive(Debug, Clone, Default)]
pub struct Part {
    pub name: String,
    pub kind: String,
    pub node: i64,
    pub verts: Vec<V3>,
    pub tris: Vec<[u32; 3]>,
    pub uv: Vec<[f64; 2]>,
    /// vertex colours r,g,b,a (render parts only)
    pub vc: Vec<[u8; 4]>,
    pub nrm: Vec<V3>,
    /// tangent xyz + handedness
    pub tan: Vec<[f64; 4]>,
    pub mat: Option<i64>,
    /// walk class ('' | ip | shield | nocollide | hull | nonplayer); None on render parts without one
    pub cls: Option<String>,
    /// physics group / geometry-header surface index (collision parts)
    pub group: Option<u32>,
    pub sid: Option<u32>,
}

/// [part] or [part with the player-blocking triangles, a copy with the 'nonplayer' ones]; a part that already has a class (ip,
/// shield, nocollide: the player passes it anyway) is not split, nor one without per-triangle materials.
pub fn split_by_surface(part: Part, tri_mats: Option<&Vec<u8>>, mtl_path: Option<&str>, materials: &mut MatRegistry) -> Vec<Part> {
    let cls_ok = matches!(part.cls.as_deref(), None | Some("") | Some(COMPANION_CLASS));
    let (Some(tm), Some(mp)) = (tri_mats, mtl_path) else { return vec![part] };
    if !cls_ok || tm.is_empty() || mp.is_empty() || tm.len() != part.tris.len() {
        return vec![part];
    }
    let mut passes: HashMap<u8, bool> = HashMap::new();
    for &m in tm.iter().collect::<HashSet<_>>() {
        passes.insert(m, !materials.player_collides(mp, m as i64));
    }
    if !passes.values().any(|&v| v) {
        return vec![part];
    }
    let (mut keep, mut gone) = (vec![], vec![]);
    for (t, m) in part.tris.iter().zip(tm.iter()) {
        if passes[m] {
            gone.push(*t);
        } else {
            keep.push(*t);
        }
    }
    let mut out = vec![];
    if !keep.is_empty() {
        out.push(Part { tris: keep, ..part.clone() });
    }
    out.push(Part { tris: gone, cls: Some(SURFACE_CLASS.into()), ..part });
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// PROXY_RE = `collision|proxy|physic|_coll\b|^coll` (case-insensitive) searched in `s`.
fn proxy_match(s: &str) -> bool {
    let l = s.to_lowercase();
    if l.contains("collision") || l.contains("proxy") || l.contains("physic") || l.starts_with("coll") {
        return true;
    }
    let mut from = 0;
    while let Some(p) = l[from..].find("_coll") {
        let end = from + p + 5;
        if l[end..].chars().next().map_or(true, |c| !is_word(c)) {
            return true;
        }
        from += p + 1;
    }
    false
}

/// Python `str.splitlines()` (line breaks: \n \r \r\n \v \f \x1c-\x1e \x85    ).
fn splitlines(s: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push(&s[start..i]);
            let mut next = i + c.len_utf8();
            if c == '\r' && matches!(it.peek(), Some((_, '\n'))) {
                it.next();
                next += 1;
            }
            start = next;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// True when a node's props text marks it as a collision proxy.  The text is `key=value` lines; the joint-parameter block that
/// every animated part carries (rotLimit*, springTension*, `proxyParams=` ...) holds an EMPTY `proxyParams=` line that PROXY_RE
/// matched, so the render geometry of any node with that block (missile fins, laser barrels, turret parts) was classed as a proxy
/// and dropped from the weapon pages.  EMPIRICAL (1,500 random spaceship .cga/.cgf: the only props line matching PROXY_RE is
/// `proxyParams=`, always empty, 5 times).  An assignment with an empty value says nothing, so only lines with a value (or free
/// text) are tested.
pub fn props_say_proxy(props: &str) -> bool {
    for line in splitlines(props) {
        if let Some((_, val)) = line.split_once('=') {
            if val.trim().is_empty() {
                continue;
            }
        }
        if proxy_match(line) {
            return true;
        }
    }
    false
}

pub const IDENT: [f64; 12] = ivo::IDENT34;

/// Mesh file extensions to try for a geometry file, its own first: '.cgam' for a .cga, '.cgfm' for a .cgf.
///
/// Why: 7,651 stems of the archive exist as BOTH a .cga and a .cgf (e.g. orig_890j_sofa_length, simpod_1_a), each with its own
/// mesh file, and the two meshes differ for 3,012 of them (EMPIRICAL, whole archive: box or vertex count).  The .cgfm was always
/// tried first, so a .cga with a .cgf sibling was drawn with the .cgf's mesh under its own nodes: the 890 Jump crew-quarters sofa
/// came out turned 90 degrees against its collision and its seat helpers.  The other file is only a fallback (14 .cga files have
/// nothing but a .cgfm, 3 .cgf files nothing but a .cgam).  INFERRED engine rule: the mesh file is the geometry file name + 'm'
/// (the loader appends it; not traced).
pub fn companion_exts(name: &str) -> [&'static str; 2] {
    if name.to_lowercase().ends_with(".cga") {
        [".cgam", ".cgfm"]
    } else {
        [".cgfm", ".cgam"]
    }
}

/// Entry of the mesh file of geometry file `name` (see companion_exts), or None.
pub fn mesh_companion<'a>(p4k: &'a P4k, name: &str) -> Option<&'a Entry> {
    let stem = &name[..name.rfind('.').unwrap_or(name.len())];
    companion_exts(name).iter().find_map(|e| p4k.get(&format!("{}{}", stem, e)))
}

/// Sorted .cgf / .cga entries matching `needle` (exact stem first, LOD files last, shortest path first).
pub fn pick_assets<'a>(p4k: &'a P4k, needle: &str) -> Vec<&'a Entry> {
    let mut hits: Vec<&Entry> = p4k.find(needle, &[".cgf", ".cga"]).collect();
    let low = needle.to_lowercase();
    let want = low.rsplit('/').next().unwrap().rsplit_once('.').map_or(low.rsplit('/').next().unwrap(), |x| x.0).to_string();
    let key = |e: &Entry| {
        let n = e.name.to_lowercase();
        let file = n.rsplit('/').next().unwrap();
        let base = file.rsplit_once('.').map_or(file, |x| x.0);
        (is_lod_stem(base), base != want, n.len(), n)
    };
    hits.sort_by_key(|e| key(e));
    hits
}

/// `_lod\d+$` on a stem.
fn is_lod_stem(base: &str) -> bool {
    let t = base.trim_end_matches(|c: char| c.is_ascii_digit());
    t.len() < base.len() && t.ends_with("_lod")
}

/// Material path (no extension) stored in the cgf's material chunk, or None.
pub fn material_path(data: &[u8]) -> p4k_formats::Result<Option<String>> {
    let f = ivo::IvoFile::new(data)?;
    let Some(chunk) = f.get(ivo::CH_CGF_MATERIALS) else { return Ok(None) };
    let raw = f.bytes(chunk);
    let raw = raw.split(|&b| b == 0).next().unwrap_or(&[]);
    Ok(normalise_material_name(&String::from_utf8_lossy(raw)))
}

/// Material name as stored in a cgf -> path without extension that the lookups expect.
///
/// EMPIRICAL: 218 cgf/cga files store '/Objects/...mtl' (leading slash, and usually the '.mtl' suffix); the engine rule is
/// 'Data/' + name, so the slash is dropped and the suffix removed.  Without this the lookup
/// became 'data//objects/...' and 32 geology materials of one station looked missing.
pub fn normalise_material_name(name: &str) -> Option<String> {
    let mut n = name.replace('\\', "/").trim_start_matches('/').to_string();
    if n.to_lowercase().ends_with(".mtl") {
        n.truncate(n.len() - 4);
    }
    if n.is_empty() {
        None
    } else {
        Some(n)
    }
}

/// (material path, note) for the name stored in a cgf's material chunk.
///
/// Engine rule (CONFIRMED): a name without '/' is relative to
/// the cgf's own folder; a name with '/' gets 'Data/' prepended; an unresolved name gives the default material unless an
/// attachment (.cdf Material=) or the item record (Game2.dcb) supplies one.  Attachments are not read.  Without a record, a unique
/// same-named .mtl elsewhere in the archive is used (best-effort guess, not the engine's rule; the sibling male_v7 / female_v2
/// folder wins when there are several).  When the stored name does not resolve, `items` (the Game2.dcb item records) supplies the
/// material they pair with this geometry (several variants are common: the commonest existing one is used and the note says so).
pub fn resolve_material_path(p4k: &P4k, cgf_name: &str, mtl: Option<&str>, items: Option<&dyn ItemMaterialSource>) -> (Option<String>, Option<String>) {
    let Some(mtl) = mtl else { return (None, None) };
    let folder = cgf_name.rsplit_once('/').map_or(cgf_name, |x| x.0);
    let stored = if mtl.contains('/') { mtl.to_string() } else { format!("{}/{}", folder, mtl) };
    if p4k.find_path(&format!("{}.mtl", stored), ".mtl").is_some() {
        return (Some(stored), None);
    }
    if let Some(items) = items {
        let found: Vec<String> = items.candidates(cgf_name).into_iter().filter(|c| p4k.find_path(&format!("{}.mtl", c), ".mtl").is_some()).collect();
        if let Some(first) = found.first() {
            let note = (found.len() > 1).then(|| {
                format!(
                    "material '{}' not found; the item records (Game2.dcb) pair this model with {} materials, using the commonest: {}",
                    mtl.rsplit('/').next().unwrap(),
                    found.len(),
                    first.rsplit('/').next().unwrap()
                )
            });
            return (Some(first.clone()), note);
        }
    }
    if mtl.contains('/') {
        return (Some(mtl.to_string()), None);
    }
    let idx = p4k.basename_index(".mtl");
    let mut paths: Vec<String> = idx.get(&format!("{}.mtl", mtl.to_lowercase())).cloned().unwrap_or_default();
    if paths.len() > 1 {
        let swapped = folder.to_lowercase().replace("female_v2", "\0").replace("male_v7", "female_v2").replace('\0', "male_v7");
        let same: Vec<String> = paths.iter().filter(|x| x.rsplit_once('/').map_or(x.as_str(), |y| y.0) == swapped).cloned().collect();
        if !same.is_empty() {
            paths = same;
        }
    }
    if paths.len() == 1 {
        let p = &paths[0];
        return (
            Some(p[..p.len() - 4].to_string()),
            Some(format!(
                "material '{}' is not in the model's folder and no item record names one; using the same-named {} (best-effort guess)",
                mtl, p
            )),
        );
    }
    (Some(mtl.to_string()), Some(format!("material file not found in the model's folder or elsewhere: {}", mtl)))
}

/// Archive name of the base model of a '<stem>_lodN.<ext>' file, or None.  EMPIRICAL (AEGS_Idris.cga): the _lod1 .. _lod3 files
/// have 0 nodes; drawn with identity node matrices their parts spread over -87..46 m in x where the base spans -63..63 (the parts
/// were scattered / missing whenever the viewer switched to a LOD); with the base nodes they line up.
/// Regex `_lod\d+(\.[A-Za-z]+)$` (case-insensitive), first match.
pub fn lod_base(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    let (stem, ext) = (&name[..dot], &name[dot..]);
    if ext.len() < 2 || !ext[1..].chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let digits = stem.len() - stem.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || stem.len() < digits + 4 {
        return None;
    }
    let cut = stem.len() - digits - 4;
    if !stem[cut..cut + 4].eq_ignore_ascii_case("_lod") {
        return None;
    }
    Some(format!("{}{}", &stem[..cut], ext))
}

#[derive(Debug, Clone, Default)]
pub struct Info {
    pub asset: String,
    pub companion: Option<String>,
    pub nodes: usize,
    pub material: Option<String>,
    pub notes: Vec<String>,
    pub render_tris: Option<usize>,
    pub render_verts: Option<usize>,
    pub render_subsets: Option<usize>,
    pub collision_counts: BTreeMap<String, usize>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub with_render: bool,
    pub with_collision: bool,
    pub max_render_tris: Option<usize>,
    pub mtl_override: Option<String>,
    pub with_frames: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { with_render: true, with_collision: true, max_render_tris: None, mtl_override: None, with_frames: true }
    }
}

fn node_world(nodes: &[Node], i: i64) -> [f64; 12] {
    if i >= 0 && (i as usize) < nodes.len() {
        nodes[i as usize].world
    } else {
        IDENT
    }
}

/// Where a geometry's bytes come from: the archive, plus an optional `Overlay` of virtual files (designer brushes stored inside a
/// `.socpak`, named `<socpak entry>#<member>`, see `overlay`).  The overlay is looked at first, like `OverlayP4k`.
#[derive(Clone, Copy)]
pub struct GeoSource<'a> {
    pub p4k: &'a P4k,
    pub overlay: Option<&'a Overlay>,
}

impl<'a> GeoSource<'a> {
    pub fn new(p4k: &'a P4k) -> Self {
        GeoSource { p4k, overlay: None }
    }

    pub fn with_overlay(p4k: &'a P4k, overlay: &'a Overlay) -> Self {
        GeoSource { p4k, overlay: Some(overlay) }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.overlay.map_or(false, |o| o.contains(name)) || self.p4k.contains(name)
    }

    /// Bytes of an overlay file or archive entry, None when neither exists; Err when an archive entry cannot be read.
    pub fn read(&self, name: &str) -> Option<Result<Vec<u8>, String>> {
        if let Some(v) = self.overlay.and_then(|o| o.get(name)) {
            return Some(Ok(v.data.as_ref().clone()));
        }
        self.p4k.get(name).map(|e| self.p4k.read(e).map_err(|e| e.to_string()))
    }

    /// Mesh companion name of geometry `name` (see `companion_exts`), if one exists in the overlay or the archive.
    pub fn companion(&self, name: &str) -> Option<String> {
        let stem = &name[..name.rfind('.').unwrap_or(name.len())];
        companion_exts(name).iter().map(|e| format!("{}{}", stem, e)).find(|n| self.contains(n))
    }
}

/// Builds the scene of one .cgf / .cga entry.  `materials`: a registry shared between assets.
pub fn build_scene(p4k: &P4k, entry: &Entry, opts: &Options, materials: &mut MatRegistry) -> Result<(Info, Vec<Part>), String> {
    build_scene_named(&GeoSource::new(p4k), &entry.name, opts, materials)
}

/// `build_scene` for a geometry named `name` in `src`: an archive entry or a virtual file of the overlay (a designer brush, the
/// bytes-based entry point the zone compile needs).
pub fn build_scene_named(src: &GeoSource, name: &str, opts: &Options, materials: &mut MatRegistry) -> Result<(Info, Vec<Part>), String> {
    let p4k = src.p4k;
    let e2s = |e: p4k_formats::Error| e.to_string();
    let data = src.read(name).ok_or_else(|| format!("KeyError: {}", name))??;
    let cgf = ivo::parse_cgf(&data, true).map_err(e2s)?;
    let (mut mtl_path, mut mtl_note);
    if let Some(ov) = &opts.mtl_override {
        // a render node's own material replaces the one stored in the cgf (the engine's brush sets it as its material)
        mtl_path = normalise_material_name(ov);
        mtl_note = None;
    } else {
        let stored = material_path(&data).map_err(e2s)?;
        (mtl_path, mtl_note) = resolve_material_path(p4k, name, stored.as_deref(), materials.items);
        if let (None, Some(base_name)) = (&mtl_path, lod_base(name)) {
            if let Some(Ok(bd)) = src.read(&base_name) {
                // EMPIRICAL (toilet_roll_a_lod1..3, the Idris hangar): a _lodN file has no material chunk at all, the engine draws
                // it with the base model's material.  Without this every LOD was drawn as the flat default colour, which showed up
                // as untextured pale panels as soon as the viewer picked a LOD (view-dependent, since LODs are picked by distance /
                // triangle density).
                if let Ok(mp) = material_path(&bd) {
                    (mtl_path, mtl_note) = resolve_material_path(p4k, &base_name, mp.as_deref(), materials.items);
                }
            }
        }
    }
    let companion = src.companion(name);
    let comp_data = match &companion {
        Some(c) => Some(src.read(c).unwrap()?),
        None => None,
    };
    let meshes = match &comp_data {
        Some(d) => ivo::parse_cgfm(d).map_err(e2s)?,
        None => vec![],
    };
    let mut nodes = cgf.nodes.clone();
    if nodes.is_empty() {
        // a LOD file carries no node table: its parts are placed with the base model's nodes (same indices)
        if let Some(Ok(bd)) = lod_base(name).and_then(|b| src.read(&b)) {
            if let Ok(c) = ivo::parse_cgf(&bd, false) {
                nodes = c.nodes;
            }
        }
    }
    let mut info = Info {
        asset: name.to_string(),
        companion: companion.clone(),
        nodes: nodes.len(),
        material: mtl_path.clone(),
        notes: mtl_note.into_iter().collect(),
        ..Default::default()
    };
    let mut parts: Vec<Part> = vec![];
    if opts.with_render && !meshes.is_empty() {
        parts.extend(render_parts(&meshes[0], &nodes, mtl_path.as_deref(), materials, &mut info, opts.with_frames)?);
        if meshes.len() > 1 {
            info.notes.push(format!("{} meshes in the mesh file, only the first is drawn", meshes.len()));
        }
        if let Some(max) = opts.max_render_tris {
            if max > 0 && info.render_tris.unwrap_or(0) > max {
                let (mut keep, mut budget) = (vec![], 0usize);
                for pt in parts {
                    if pt.kind == "proxy" || budget + pt.tris.len() <= max {
                        budget += pt.tris.len();
                        keep.push(pt);
                    }
                }
                info.notes.push(format!("render meshes truncated to {} tris", budget));
                parts = keep;
            }
        }
    } else if opts.with_render && meshes.is_empty() {
        info.notes.push("no mesh companion file found (stub / lod-less cgf?)".into());
    }

    let mut errs = vec![];
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    if opts.with_collision {
        let proxied: HashSet<u16> = cgf.physics.iter().filter(|(e, g, _)| g.is_some() && e.group != ivo::PHYS_FIELD).map(|(e, _, _)| e.node).collect();
        let clustered: HashSet<u16> = cgf.physics.iter().filter(|(e, g, _)| g.is_some() && e.group == ivo::PHYS_CLUSTER).map(|(e, _, _)| e.node).collect();
        for (e, g, err) in &cgf.physics {
            if e.group == ivo::PHYS_FIELD && g.is_some() && proxied.contains(&e.node) {
                info.notes.push("distance-field hull skipped: node has regular collision geometry".into());
                continue;
            }
            let Some(g) = g else {
                errs.push(format!("group {} entry {}: {}", e.group, e.index, err.clone().unwrap_or_default()));
                continue;
            };
            let Some((mverts, mtris)) = geom_to_mesh(g) else {
                errs.push(format!("group {} entry {}: {}", e.group, e.index, g.kind_name()));
                continue;
            };
            let world = if e.node != 0xFFFF { node_world(&nodes, e.node as i64) } else { IDENT };
            let verts: Vec<V3> = mverts.iter().map(|v| ivo::xform(&world, *v)).collect();
            let nd = nodes.get(e.node as usize);
            let mut name = nd.map_or("(object)".to_string(), |n| n.name.clone());
            if e.proxy != 0 {
                name += &format!(" [proxy {}]", e.proxy - 1);
            }
            let mut cls = nd.map_or("", |n| collision_class(&n.name, n.phys_flags, &n.props));
            cls = companion_class(cls, e.group, clustered.contains(&e.node));
            let part = Part {
                name,
                kind: g.kind_name(),
                node: if e.node != 0xFFFF { e.node as i64 } else { -1 },
                verts,
                tris: mtris,
                cls: Some(cls.to_string()),
                group: Some(e.group),
                sid: g.surface_idx,
                ..Default::default()
            };
            parts.extend(split_by_surface(part, g.mat.as_ref(), mtl_path.as_deref(), materials));
            *counts.entry(g.kind_name()).or_default() += 1;
        }
    }
    info.collision_counts = counts;
    info.errors = errs;
    Ok((info, parts))
}

fn moves(w: &[f64; 12]) -> bool {
    (0..3).any(|i| (0..4).any(|j| (w[i * 4 + j] - if i == j { 1.0 } else { 0.0 }).abs() > 1e-4))
}

/// [matrix per subset of m]: what moves the subset's vertices into object space.  `ivo::geometry_matrix` of the subset's node,
/// except that a subset whose node matrix would carry its vertices out of the mesh's own object box while the raw vertices lie
/// inside it keeps them as stored (identity).
///
/// Why: the mesh header box (Mesh.bbox_*) is the object-space box of every vertex (EMPIRICAL: reproduced for
/// 1,554 of 2,024 files with moving nodes).  Some subsets hold vertices that are already in object space although they are tagged
/// with a moved node: orig_890j_crew_quarters_changing.cgf tags the three 'glow_toilet_sign' meshes (30 triangles, sub-material
/// 43, raw x -8.47 .. -3.38 at y 1.78: their object-space spot, inside the header box x -11.47 .. 0.04) with its main node, which
/// is turned 90 deg about z; with that node's matrix the signs went to x 1.78, y 3.4 .. 8.5, out of the box and through the wall.
/// The decision is per subset because the other subsets of that node are node-local (applying the matrix keeps them inside the
/// box).  EMPIRICAL rule; the engine flag behind it is not known.  Applied only when the header box is plausible;
/// when both or neither choice fits, the node rule stands.
pub fn vertex_matrices(m: &ivo::Mesh, nodes: &[Node], pos: &[V3], idx: &[u16]) -> Vec<[f64; 12]> {
    let tol = 0.02;
    let (lo, hi) = (m.bbox_min, m.bbox_max);
    let ok = lo.iter().chain(hi.iter()).all(|x| x.abs() < 1e6) && (0..3).all(|k| lo[k] <= hi[k]);
    let slack = if ok { tol + 0.001 * ((0..3).map(|k| (hi[k] - lo[k]).powi(2)).fold(0.0, |a, b| a + b)).powf(0.5) } else { 0.0 };
    let mut out = Vec::with_capacity(m.subsets.len());
    for s in &m.subsets {
        let ni = (s.mat_id >> 16) as i64;
        let node = nodes.get(ni as usize);
        let mut w = if node.is_some() { ivo::geometry_matrix(node) } else { node_world(nodes, ni) };
        if ok && moves(&w) {
            let a = (s.first_index as usize).min(idx.len());
            let seg = &idx[a..(a + s.num_indices as usize).min(idx.len())];
            let base = ivo::subset_index_base(s, seg);
            let (mut raw_out, mut moved_out) = (0.0f64, 0.0f64);
            let uniq: HashSet<u16> = seg.iter().copied().collect();
            for vi in uniq {
                let k = base + vi as i64;
                if k < 0 || k as usize >= pos.len() {
                    continue;
                }
                let p = pos[k as usize];
                let q = ivo::xform(&w, p);
                for c in 0..3 {
                    raw_out = raw_out.max(lo[c] - p[c]).max(p[c] - hi[c]);
                    moved_out = moved_out.max(lo[c] - q[c]).max(q[c] - hi[c]);
                }
            }
            if moved_out > slack && raw_out <= slack {
                w = IDENT;
            }
        }
        out.push(w);
    }
    out
}

/// Render meshes grouped by (owning node, sub-material); vertices are moved into the object's space.
fn render_parts(m: &ivo::Mesh, nodes: &[Node], mtl_path: Option<&str>, materials: &mut MatRegistry, info: &mut Info, with_frames: bool) -> Result<Vec<Part>, String> {
    let e2s = |e: p4k_formats::Error| e.to_string();
    let pos = m.positions().map_err(e2s)?;
    let uvs = m.uvs().map_err(e2s)?;
    let cols = m.colors().map_err(e2s)?;
    let idx = m.indices().map_err(e2s)?;
    let frames = if with_frames { FrameReader::new(m) } else { None };
    let smats = vertex_matrices(m, nodes, &pos, &idx);
    let mut groups: BTreeMap<(u32, u32), Vec<(usize, [f64; 12])>> = BTreeMap::new();
    for (si, s) in m.subsets.iter().enumerate() {
        groups.entry((s.mat_id >> 16, s.mat_id & 0xFFFF)).or_default().push((si, smats[si]));
    }
    let mut parts = vec![];
    let mut ntri = 0usize;
    for ((ni, sub), subs) in &groups {
        let node = nodes.get(*ni as usize);
        let gmat = match mtl_path {
            Some(mp) => materials.resolve(mp, *sub as i64),
            None => -1,
        };
        let nodraw = materials.is_nodraw(gmat);
        let proxy = nodraw || node.map_or(false, |n| proxy_match(&n.name) || props_say_proxy(&n.props));
        let same = subs.iter().all(|(_, w)| *w == subs[0].1);
        let mut dense: Vec<u32> = if same { vec![u32::MAX; pos.len()] } else { vec![] };
        let mut sparse: HashMap<(i64, usize), u32> = HashMap::new();
        let (mut verts, mut uv, mut vc, mut tris): (Vec<V3>, Vec<[f64; 2]>, Vec<[u8; 4]>, Vec<[u32; 3]>) = (vec![], vec![], vec![], vec![]);
        let (mut nrm, mut tan): (Vec<V3>, Vec<[f64; 4]>) = (vec![], vec![]);
        // helper nodes do not move their vertices; subsets stored in object space neither (vertex_matrices)
        for (j, (si, world)) in subs.iter().enumerate() {
            let s = &m.subsets[*si];
            let a = (s.first_index as usize).min(idx.len());
            let base = ivo::subset_index_base(s, &idx[a..(a + s.num_indices as usize).min(idx.len())]);
            for t in 0..(s.num_indices / 3) as usize {
                let mut tri = [0u32; 3];
                for (k, slot) in tri.iter_mut().enumerate() {
                    let ii = s.first_index as usize + 3 * t + k;
                    // absolute for small meshes, subset-relative for big ones
                    let vi = base + *idx.get(ii).ok_or("IndexError: tuple index out of range")? as i64;
                    if vi < 0 || vi as usize >= pos.len() {
                        return Err("IndexError: tuple index out of range".into());
                    }
                    let vu = vi as usize;
                    let existing = if same { dense[vu] } else { sparse.get(&(vi, j)).copied().unwrap_or(u32::MAX) };
                    *slot = if existing != u32::MAX {
                        existing
                    } else {
                        let n = verts.len() as u32;
                        if same {
                            dense[vu] = n;
                        } else {
                            sparse.insert((vi, j), n);
                        }
                        verts.push(ivo::xform(world, pos[vu]));
                        uv.push(uvs.get(vu).copied().unwrap_or([0.0, 0.0]));
                        vc.push(cols.get(vu).map_or([0xFF; 4], |c| ivo::bgra_to_rgba(*c)));
                        if let Some(fr) = &frames {
                            let f: Frame = ivo::rotate_frame(world, fr.get(vu).map_err(e2s)?);
                            nrm.push(f.n);
                            tan.push([f.t[0], f.t[1], f.t[2], f.h]);
                        }
                        n
                    };
                }
                tris.push(tri);
            }
        }
        ntri += tris.len();
        let mut part = Part {
            name: node.map_or("mesh".to_string(), |n| n.name.clone()),
            kind: if proxy { "proxy" } else { "render" }.to_string(),
            node: *ni as i64,
            verts,
            tris,
            uv,
            vc,
            ..Default::default()
        };
        if frames.is_some() {
            part.nrm = nrm; // the file's own tangent frame (ivo::FrameReader), moved with the vertices
            part.tan = tan;
        }
        if gmat >= 0 {
            part.mat = Some(gmat);
        }
        if let (true, Some(mp)) = (proxy, mtl_path) {
            if !materials.player_collides(mp, *sub as i64) {
                part.cls = Some(SURFACE_CLASS.into()); // e.g. NoDraw 'shield_proxy' faces with a shield surface (see split_by_surface)
            }
        }
        parts.push(part);
    }
    info.render_tris = Some(ntri);
    info.render_verts = Some(m.n_verts);
    info.render_subsets = Some(m.subsets.len());
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_regex_rules() {
        for s in ["Collision_main", "hull_proxy", "physic_01", "wing_coll", "wing_coll.001", "COLL_x", "x_coll_y"] {
            // 'x_coll_y': '_coll' is followed by '_', a word char, so no \b there
            let want = s != "x_coll_y";
            assert_eq!(proxy_match(s), want, "{}", s);
        }
        assert!(!proxy_match("recoil"));
        assert!(!proxy_match("my_collar"));
    }

    #[test]
    fn empty_proxy_params_line_is_ignored() {
        assert!(!props_say_proxy("rotLimit=1\r\nproxyParams=\r\nspringTension=2"));
        assert!(props_say_proxy("rotLimit=1\nproxyParams=1"));
        assert!(props_say_proxy("physics"));
    }

    #[test]
    fn collision_classes() {
        assert_eq!(collision_class("x", 0x20, ""), "ip");
        assert_eq!(collision_class("x", 0x10, ""), "shield");
        assert_eq!(collision_class("x", 0x40, ""), "nocollide");
        assert_eq!(collision_class("x", 0, "No_Player_Collide"), "nocollide");
        assert_eq!(collision_class("$IP_door", 0, ""), "ip");
        assert_eq!(collision_class("$shield_a", 0, "player_only_collision"), "");
        assert_eq!(collision_class("a_no_collide", 0, ""), "nocollide");
        assert_eq!(collision_class("wall", 0, ""), "");
        assert_eq!(companion_class("", ivo::PHYS_GEOM, true), "hull");
        assert_eq!(companion_class("ip", ivo::PHYS_GEOM, true), "ip");
    }

    #[test]
    fn lod_names() {
        assert_eq!(lod_base("a/b/thing_lod2.cga").as_deref(), Some("a/b/thing.cga"));
        assert_eq!(lod_base("a/b/thing.cga"), None);
        assert_eq!(lod_base("a/thing_lod.cga"), None);
        assert!(is_lod_stem("x_lod12") && !is_lod_stem("x_lod") && !is_lod_stem("x12"));
    }

    #[test]
    fn material_names() {
        assert_eq!(normalise_material_name("/Objects/a/b.mtl").as_deref(), Some("Objects/a/b"));
        assert_eq!(normalise_material_name("b").as_deref(), Some("b"));
        assert_eq!(normalise_material_name(""), None);
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
    }
}
