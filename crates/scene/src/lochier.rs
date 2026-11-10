//! Location hierarchy for the loot page: system -> planet -> moon -> (station / outpost) -> locations.
//!
//! Evidence (EMPIRICAL, live archive, 2026-11):
//! * A loot location is an object container (`.socpak`), not a StarMapObject; no record links the two.  The only link is the container
//!   tree: the bodies are containers `Data/ObjectContainers/PU/system/<system>/<body>.socpak` (`stanton2b.socpak`, `pyro5a.socpak`,
//!   `nyx1.socpak`), the file stem equals the StarMapObject record name (`StarMapObject.Stanton2b` = Daymar), and the module containers
//!   are reached through `ChildObjectContainers` chains.  [`placements`] therefore climbs the reverse child lists from a location to every
//!   container it is used in and collects the body stems on the way.  Generic prefabs (`PU/loc/mod/...`) are used on several bodies and are
//!   then listed under each of them.
//! * Names: `StarMapObject.name` through global.ini.  Parent chain: `StarMapObject.parent` (planet -> star, moon -> planet).
//! * Pictures: `StarMapObject.assetManagerLocationParams.previewImagePath` / `previewIconPath` (NikNax asset manager images, `.tif` in the
//!   record, `.dds` in the archive).  Bodies without one get no picture.
//!
//! The tree builder is pure (bodies + placements in, JSON out) so it is unit-tested with a synthetic hierarchy.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use p4k_formats::socpak::SocZip;
use p4k::P4k;
use serde_json::{json, Value};

use crate::itemmaterials::ItemMaterials;
use crate::itemstats::ItemStats;

/// One StarMapObject record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Body {
    /// short record name (`Stanton2b`)
    pub id: String,
    /// localised display name (falls back to the id)
    pub label: String,
    /// short name of `parent`, if any
    pub parent: Option<String>,
    /// `navIcon` (Planet, Moon, Star, Station, Outpost, ...), lower case
    pub kind: String,
    /// archive path (`.dds`) of the asset manager preview image, if the record names one
    pub preview: Option<String>,
    /// archive path (`.dds`) of the asset manager list icon
    pub icon: Option<String>,
    /// local file of a picture from the Star Citizen wiki (system maps only), when the user has fetched it
    pub wiki: Option<String>,
}

fn dds_path(tif: &str) -> Option<String> {
    let t = tif.trim().replace('\\', "/");
    if t.is_empty() {
        return None;
    }
    let t = t.trim_start_matches('/');
    let stem = t.rsplit_once('.').map_or(t, |x| x.0);
    let full = if t.to_lowercase().starts_with("data/") { stem.to_string() } else { format!("Data/{}", stem) };
    Some(format!("{}.dds", full))
}

/// Folder of the pictures fetched from the Star Citizen wiki: `$P4K_BODYPICS` or `~/.cache/p4ktools/bodypics` (kept out of the repo on purpose).
pub fn wiki_dir() -> Option<std::path::PathBuf> {
    let d = std::env::var_os("P4K_BODYPICS").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".cache/p4ktools/bodypics")))?;
    d.is_dir().then_some(d)
}

/// All StarMapObject records (localised names, parents, pictures).
pub fn read_bodies(items: &ItemMaterials, stats: &ItemStats, p4k: &P4k) -> Vec<Body> {
    let Some(d) = items.datacore() else { return vec![] };
    let mut out = vec![];
    for (name, ids) in d.record_index() {
        let Some(short) = name.strip_prefix("StarMapObject.") else { continue };
        for &i in ids {
            let Ok(rec) = d.record(i) else { continue };
            let Ok(data) = d.record_data(&rec, 3) else { continue };
            let data = data.to_json();
            let st = |k: &str| data[k].as_str().unwrap_or("").to_string();
            let label = stats.localise_str(&st("name")).filter(|x| !x.is_empty() && !x.contains("PLACEHOLDER")).unwrap_or_else(|| short.to_string());
            let parent = data["parent"]
                .as_array()
                .filter(|a| a.len() == 3 && a[0] == "ref")
                .and_then(|a| a[2].as_str())
                .and_then(|g| items.guid_record(g))
                .and_then(|pi| d.record_name(pi).ok().map(|n| n.split_once('.').map_or(n.to_string(), |x| x.1.to_string())));
            let am = &data["assetManagerLocationParams"];
            let pic = |k: &str| am[k].as_str().and_then(dds_path).filter(|p| p4k.get(p).is_some());
            out.push(Body { id: short.to_string(), label, parent, kind: st("navIcon").to_lowercase(), preview: pic("previewImagePath"), icon: pic("previewIconPath"), wiki: None });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    // wiki pictures of the systems (parentless bodies with no archive picture): `<system key>.png` in the picture folder
    if let Some(dir) = wiki_dir() {
        for b in out.iter_mut().filter(|b| b.parent.is_none() && b.preview.is_none() && b.icon.is_none()) {
            let f = dir.join(format!("{}.png", system_key(b)));
            if f.is_file() {
                b.wiki = Some(f.to_string_lossy().into_owned());
            }
        }
    }
    out
}

/// Child -> parents over every container of the archive (key = [`crate::loot::container_key`]; the values are archive entry names).
/// Reads every container once (seconds), so callers cache it.
pub fn parent_map(p4k: &P4k) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for e in p4k.entries.iter().filter(|e| e.name.to_lowercase().ends_with(".socpak") && e.name.contains("ObjectContainers")) {
        let Ok(data) = p4k.read(e) else { continue };
        let Ok(z) = SocZip::open(&data) else { continue };
        for c in p4k_formats::socpak::read_child_containers(&z) {
            if let Some(path) = c.container.as_deref() {
                out.entry(crate::loot::container_key(path)).or_default().push(e.name.to_string());
            }
        }
    }
    out
}

/// Lower-case segments of a container path below `ObjectContainers/PU/` (file stem for the last one).
fn segments(path: &str) -> Vec<String> {
    let k = crate::loot::container_key(path);
    let mut v: Vec<String> = k.split('/').map(String::from).collect();
    if let Some(l) = v.last_mut() {
        *l = l.trim_end_matches(".socpak").to_string();
    }
    v
}

/// The body stems and the system a container path names: bodies live in `.../system/<system>/<stem>(.socpak|/)`, systems also show as
/// `.../loc/(mod|flagship)/<system>/`.  `known` maps lower-case record names to themselves; `systems` are lower-case system keys.
pub fn path_places(path: &str, known: &HashSet<String>, systems: &HashSet<String>) -> (Vec<String>, Option<String>) {
    let seg = segments(path);
    let mut bodies = vec![];
    let mut sys = None;
    for (i, s) in seg.iter().enumerate() {
        if systems.contains(s) && i > 0 && matches!(seg[i - 1].as_str(), "system" | "mod" | "flagship") {
            sys = Some(s.clone());
            // `system/<sys>/<stem>`: the stem right below the system folder, or any deeper folder named like a body
            if seg[i - 1] == "system" {
                if let Some(stem) = seg.get(i + 1) {
                    bodies.extend(stem_bodies(stem, known));
                }
            }
        }
    }
    bodies.sort();
    bodies.dedup();
    (bodies, sys)
}

/// `stanton2b` -> `stanton2b`; `stanton4_planet_only` -> `stanton4`; `stanton2_l1` -> itself when a record has that name.
fn stem_bodies(stem: &str, known: &HashSet<String>) -> Vec<String> {
    if known.contains(stem) {
        return vec![stem.to_string()];
    }
    // longest prefix of the form <letters><digits><letter?> that is a record name
    let b = stem.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    let mut j = i;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if i == 0 || j == i {
        return vec![];
    }
    for end in [j + 1, j] {
        if end <= b.len() && stem.is_char_boundary(end) && (end == j || b[j].is_ascii_alphabetic()) && known.contains(&stem[..end]) {
            // `stanton2b_x` is Daymar-ish; a trailing letter is part of the key only when it is directly followed by a separator or the end
            if end == j + 1 && b.get(end).is_some_and(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            return vec![stem[..end].to_string()];
        }
    }
    vec![]
}

/// Placement of every location: `{location index: (bodies it is used on, system key if only the system is known)}`.  `locs` are the
/// location container paths; climbing follows `parents` (child key -> parent entry names) through every ancestor (cap `max_nodes` per location).
pub fn placements(
    locs: &[String],
    parents: &HashMap<String, Vec<String>>,
    known: &HashSet<String>,
    systems: &HashSet<String>,
) -> Vec<(BTreeSet<String>, Option<String>)> {
    let mut memo: HashMap<String, (Vec<String>, Option<String>)> = HashMap::new();
    locs.iter()
        .map(|p| {
            let mut seen: HashSet<String> = HashSet::new();
            let mut stack = vec![p.clone()];
            let mut bodies = BTreeSet::new();
            let mut sys: Option<String> = None;
            while let Some(cur) = stack.pop() {
                let key = crate::loot::container_key(&cur);
                if !seen.insert(key.clone()) || seen.len() > 20000 {
                    continue;
                }
                let (b, s) = memo.entry(key.clone()).or_insert_with(|| path_places(&cur, known, systems)).clone();
                bodies.extend(b);
                if sys.is_none() {
                    sys = s;
                }
                if let Some(ps) = parents.get(&key) {
                    stack.extend(ps.iter().cloned());
                }
            }
            (bodies, sys)
        })
        .collect()
}

/// Tree node kinds the page understands: `system`, `planet`, `moon`, `other` (stations, outposts, lagrange points ... below a body).
fn node_kind(b: &Body, depth: usize) -> &'static str {
    match (b.kind.as_str(), depth) {
        (_, 0) => "system",
        ("default", _) => "other",
        ("moon", _) => "moon",
        ("planet", _) => "planet",
        (_, 1) => "planet",
        (_, 2) => "moon",
        _ => "other",
    }
}

/// Lower-case system key of a root body (`StanttonStar`/`Stanton` -> `stanton`).
pub fn system_key(root: &Body) -> String {
    let l = root.id.to_lowercase();
    for suf in ["solarsystem", "system", "star"] {
        if let Some(x) = l.strip_suffix(suf) {
            if !x.is_empty() {
                return x.to_string();
            }
        }
    }
    l
}

/// Builds the tree: `{systems: [node], unplaced: [location index], stats}`.  `place[i]` = (bodies, system key) of location `i`.
/// A node is `{id, label, kind, img, n, locs: [index], children: [node], all: [index]}`; `n` counts the distinct locations below.  Bodies
/// without any location below them are left out.  A location whose placement names only a system sits directly under that system; one with
/// no placement is in `unplaced`.  System nodes are the star records (EMPIRICAL: `StantonStar` navIcon Star, `StantonSolarSystem` is a size
/// 0.1 holder with no children); a parentless non-star record (Nyx, GlaciemRing) is a system when no star has its key, otherwise a child
/// of the system it was seen in.
pub fn build_tree(bodies: &[Body], place: &[(BTreeSet<String>, Option<String>)]) -> Value {
    let by_id: HashMap<String, &Body> = bodies.iter().map(|b| (b.id.clone(), b)).collect();
    let lower: HashMap<String, &Body> = bodies.iter().map(|b| (b.id.to_lowercase(), b)).collect();
    let roots: Vec<&Body> = bodies.iter().filter(|b| b.parent.as_deref().is_none_or(|p| !by_id.contains_key(p))).collect();
    let mut sys_root: HashMap<String, &Body> = HashMap::new();
    for b in roots.iter().filter(|b| b.kind == "star") {
        sys_root.entry(system_key(b)).or_insert(b);
    }
    // system name: the `<X>SolarSystem` holder record says "Stanton System", the star record "Stanton system" / "Pyro"
    let mut sys_label: HashMap<String, String> = HashMap::new();
    for b in &roots {
        if b.id.to_lowercase().ends_with("solarsystem") {
            sys_label.insert(system_key(b), b.label.trim_end_matches(" System").trim_end_matches(" system").to_string());
        }
    }
    // orphan bodies (no parent record, no star of their own key) go under the system they were seen in
    let mut orphan_sys: HashMap<String, String> = HashMap::new();
    for (bs, sys) in place {
        for b in bs {
            if let (Some(body), Some(sy)) = (lower.get(b), sys) {
                if roots.iter().any(|r| r.id == body.id) && !sys_root.values().any(|r| r.id == body.id) {
                    orphan_sys.entry(body.id.clone()).or_insert_with(|| sy.clone());
                }
            }
        }
    }
    for b in &roots {
        if b.kind != "star" && !b.id.to_lowercase().ends_with("solarsystem") && !sys_root.contains_key(&system_key(b)) && !orphan_sys.contains_key(&b.id) {
            sys_root.entry(system_key(b)).or_insert(b);
        }
    }
    let mut at: BTreeMap<String, Vec<usize>> = BTreeMap::new(); // body id -> locations
    let mut unplaced = vec![];
    for (i, (bs, sys)) in place.iter().enumerate() {
        let mut put = false;
        for b in bs {
            if let Some(body) = lower.get(b) {
                at.entry(body.id.clone()).or_default().push(i);
                put = true;
            }
        }
        if !put {
            // system only: sits directly under that system
            if let Some(root) = sys.as_ref().and_then(|s| sys_root.get(s)) {
                at.entry(root.id.clone()).or_default().push(i);
            } else {
                unplaced.push(i);
            }
        }
    }
    // children lists
    let mut kids: HashMap<&str, Vec<&Body>> = HashMap::new();
    for b in bodies {
        if let Some(p) = b.parent.as_deref().filter(|p| by_id.contains_key(*p)) {
            kids.entry(p).or_default().push(b);
        }
    }
    for (id, sy) in &orphan_sys {
        if let (Some(root), Some(b)) = (sys_root.get(sy), by_id.get(id)) {
            kids.entry(root.id.as_str()).or_default().push(b);
        }
    }
    fn node(b: &Body, depth: usize, kids: &HashMap<&str, Vec<&Body>>, at: &BTreeMap<String, Vec<usize>>) -> Option<Value> {
        let mut ch: Vec<Value> = kids.get(b.id.as_str()).into_iter().flatten().filter_map(|k| node(k, depth + 1, kids, at)).collect();
        ch.sort_by(|a, b| a["label"].as_str().cmp(&b["label"].as_str()).then(a["id"].as_str().cmp(&b["id"].as_str())));
        let locs = at.get(&b.id).cloned().unwrap_or_default();
        if ch.is_empty() && locs.is_empty() {
            return None;
        }
        let mut all: BTreeSet<usize> = locs.iter().copied().collect();
        for c in &ch {
            all.extend(c["all"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|v| v as usize)));
        }
        Some(json!({"id": b.id, "label": b.label, "kind": node_kind(b, depth), "img": b.preview.as_ref().or(b.icon.as_ref()).or(b.wiki.as_ref()), "src": if b.preview.is_some() || b.icon.is_some() { "archive" } else if b.wiki.is_some() { "wiki" } else { "" }, "n": all.len(),
                    "locs": locs, "children": ch, "all": all.into_iter().collect::<Vec<_>>()}))
    }
    let mut systems: Vec<Value> = sys_root
        .iter()
        .filter_map(|(k, b)| {
            let mut v = node(b, 0, &kids, &at)?;
            if let Some(l) = sys_label.get(k) {
                v["label"] = json!(l);
            }
            Some(v)
        })
        .collect();
    systems.sort_by(|a, b| b["n"].as_u64().cmp(&a["n"].as_u64()).then(a["label"].as_str().cmp(&b["label"].as_str())));
    let placed: usize = place.len() - unplaced.len();
    json!({"systems": systems, "unplaced": unplaced, "stats": {"locations": place.len(), "placed": placed, "unplaced": unplaced.len()}})
}

/// Bodies' lower-case record names and system keys, for [`placements`].
pub fn known_sets(bodies: &[Body]) -> (HashSet<String>, HashSet<String>) {
    let known = bodies.iter().map(|b| b.id.to_lowercase()).collect();
    let systems = bodies.iter().filter(|b| b.parent.is_none()).map(system_key).collect();
    (known, systems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(id: &str, parent: Option<&str>, kind: &str) -> Body {
        Body { id: id.into(), label: id.to_uppercase(), parent: parent.map(String::from), kind: kind.into(), preview: None, icon: None, wiki: None }
    }
    fn synthetic() -> Vec<Body> {
        vec![
            body("StantonStar", None, "star"),
            body("Stanton2", Some("StantonStar"), "planet"),
            body("Stanton2b", Some("Stanton2"), "moon"),
            body("Stanton2c", Some("Stanton2"), "moon"),
            body("Stanton3", Some("StantonStar"), "planet"),
            body("PyroStar", None, "star"),
            body("PyroSolarSystem", None, "default"),
            body("Pyro5", Some("PyroStar"), "planet"),
            body("Nyx", None, "default"),
            body("Glaciem", None, "default"),
        ]
    }
    fn sets() -> (HashSet<String>, HashSet<String>) {
        known_sets(&synthetic())
    }

    #[test]
    fn dds_path_maps_tif_to_archive() {
        assert_eq!(dds_path("UI/mobiGlas/a/b.tif").as_deref(), Some("Data/UI/mobiGlas/a/b.dds"));
        assert_eq!(dds_path("Data/UI/x.TIF").as_deref(), Some("Data/UI/x.dds"));
        assert_eq!(dds_path(""), None);
    }

    #[test]
    fn stems_resolve_to_bodies() {
        let (known, systems) = sets();
        assert!(systems.contains("stanton") && systems.contains("pyro"));
        let p = |s: &str| path_places(s, &known, &systems);
        assert_eq!(p("Data/ObjectContainers/PU/system/stanton/stanton2b.socpak").0, vec!["stanton2b"]);
        assert_eq!(p("Data/ObjectContainers/PU/system/stanton/stanton2/foo/bar.socpak").0, vec!["stanton2"]);
        assert_eq!(p("Data/ObjectContainers/PU/system/stanton/stanton3_planet_only.socpak").0, vec!["stanton3"]);
        assert_eq!(p("Data/ObjectContainers/PU/system/pyro/pyro9.socpak").0, Vec::<String>::new());
        let (b, s) = p("Data/ObjectContainers/PU/loc/mod/pyro/station/x.socpak");
        assert!(b.is_empty());
        assert_eq!(s.as_deref(), Some("pyro"));
        assert_eq!(p("Data/ObjectContainers/PU/loc/mod/outpost/x.socpak"), (vec![], None));
    }

    #[test]
    fn placement_climbs_parent_chains() {
        let (known, systems) = sets();
        let mut parents = HashMap::new();
        parents.insert("objectcontainers/pu/loc/mod/a/leaf.socpak".to_string(), vec!["Data/ObjectContainers/PU/loc/mod/a/mid.socpak".to_string()]);
        parents.insert("objectcontainers/pu/loc/mod/a/mid.socpak".to_string(), vec![
            "Data/ObjectContainers/PU/system/stanton/stanton2b.socpak".to_string(),
            "Data/ObjectContainers/PU/system/stanton/stanton3.socpak".to_string(),
            // a cycle must not hang
            "Data/ObjectContainers/PU/loc/mod/a/leaf.socpak".to_string(),
        ]);
        let locs = vec!["Data/ObjectContainers/PU/loc/mod/a/leaf.socpak".to_string(), "Data/ObjectContainers/PU/loc/mod/b/lone.socpak".to_string()];
        let pl = placements(&locs, &parents, &known, &systems);
        assert_eq!(pl[0].0.iter().cloned().collect::<Vec<_>>(), vec!["stanton2b", "stanton3"]);
        assert!(pl[1].0.is_empty());
    }

    #[test]
    fn tree_groups_system_planet_moon() {
        let bodies = synthetic();
        let place = vec![
            (["stanton2b".to_string()].into_iter().collect::<BTreeSet<_>>(), None),
            (["stanton2b".to_string(), "stanton3".to_string()].into_iter().collect(), None), // used on two bodies
            (BTreeSet::new(), Some("pyro".to_string())),                                     // system only
            (BTreeSet::new(), None),                                                         // nothing known
            (["glaciem".to_string()].into_iter().collect(), Some("nyx".to_string())),        // orphan body seen in Nyx
        ];
        let t = build_tree(&bodies, &place);
        assert_eq!(t["stats"], json!({"locations": 5, "placed": 4, "unplaced": 1}));
        assert_eq!(t["unplaced"], json!([3]));
        let sys = t["systems"].as_array().unwrap();
        assert_eq!(sys.len(), 3);
        let st = &sys[0];
        assert_eq!(st["id"], "StantonStar");
        assert_eq!(st["kind"], "system");
        assert_eq!(st["n"], 2);
        let kids = st["children"].as_array().unwrap();
        let ids: Vec<&str> = kids.iter().map(|k| k["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["Stanton2", "Stanton3"]);
        let p2 = &kids[0];
        assert_eq!(p2["kind"], "planet");
        assert_eq!(p2["n"], 2);
        assert_eq!(p2["children"].as_array().unwrap().len(), 1); // Stanton2c has no location: pruned
        assert_eq!(p2["children"][0]["kind"], "moon");
        assert_eq!(p2["children"][0]["locs"], json!([0, 1]));
        let py = sys.iter().find(|x| x["id"] == "PyroStar").unwrap();
        assert_eq!(py["locs"], json!([2])); // system-only location sits under the star
        let nyx = sys.iter().find(|x| x["id"] == "Nyx").unwrap();
        assert_eq!(nyx["children"][0]["id"], "Glaciem");
        assert_eq!(nyx["children"][0]["kind"], "other");
    }

    /// Real archive: hierarchy coverage for the loot locations plus known bodies and pictures.  Skips without the archive.
    #[test]
    fn real_archive_hierarchy() {
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = P4k::open(std::path::Path::new(&path)) else { return };
        let Ok(items) = ItemMaterials::load(&p4k) else { return };
        let stats = ItemStats::new(&items, &p4k);
        let bodies = read_bodies(&items, &stats, &p4k);
        let label = |id: &str| bodies.iter().find(|b| b.id == id).map(|b| b.label.clone());
        assert_eq!(label("Stanton2b").as_deref(), Some("Daymar"));
        let with_pic = bodies.iter().filter(|b| b.preview.is_some() || b.icon.is_some()).count();
        eprintln!("{} bodies, {} with a picture", bodies.len(), with_pic);
        let dm = bodies.iter().find(|b| b.id == "Stanton2b").unwrap();
        assert_eq!(dm.parent.as_deref(), Some("Stanton2"));
        assert!(dm.preview.is_some() || dm.icon.is_some());
    }
}
