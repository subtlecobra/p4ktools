//! The interaction and elevator feeds of a zone (`k=inter`, `k=elev`): interactables, panels, remote seats,
//! panel displays, elevator cars and elevators.
//!
//! The data (doors, seats, turrets, panels, ladders, beds, elevators) and the approximations are documented in `interact.rs`,
//! `elevator.rs`, `bed.rs`, `station.rs` and `liftpath.rs`; this module only turns the tagged instances of the compile into the two
//! JSON documents the viewer reads and the info notes.  Item ids, model indices and leaf indices must match the zone's instance list
//! because the viewer addresses instances by index.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regex::Regex;
use serde_json::{json, Map, Value};

use super::OwnMaterial;
use crate::bed;
use crate::builder::{build_scene_named, GeoSource, Options};
use crate::elevator::{self, Plan};
use crate::interact::{box_xf, ladder_item, Catalog, Desc, Dg, APPROX_NOTE};
use crate::liftpath;
use crate::materials::{rnd, MatRegistry};
use crate::mfd;
use crate::panelstyle::PanelStyle;
use crate::socpak_scene::{LadderRec, Tag};
use crate::station;
use p4k_formats::ivo;

type Mat = [f64; 12];
type V3 = [f64; 3];

/// panel kinds whose EntityLinks are followed to door items
const LINK_SUBS: [&str; 2] = ["door", "elevator"];
/// m: a door panel without entity links takes the nearest door within this distance (INFERRED fallback)
const PANEL_NEAREST: f64 = 4.0;
/// m: the same for an elevator panel and a lift-named door: a lift platform's reference point is its middle, so the wall switch is farther
/// (INFERRED; Caterpillar front lift switches stand 4.5 m from it)
const LIFT_NEAREST: f64 = 6.0;
/// door_of / links scope of ship loadout items, keyed by their uid instead of an entity GUID
pub(super) const LOADOUT_SCOPE: usize = usize::MAX;

fn lift_door() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)elevator|lift|gateway").unwrap())
}

/// An instance index recorded for a moving part: `gi` = the door group / turret joint it belongs to (None for a car part).
#[derive(Debug, Clone, Copy)]
pub(super) struct Leaf {
    pub i: usize,
    pub gi: Option<usize>,
}

impl Leaf {
    fn json(&self) -> Value {
        json!({"i": self.i, "g": self.gi})
    }
}

/// A tagged entity of the compile (`split` ones have a `dg` and leaves).
pub(super) struct Tagged {
    pub tag: Tag,
    pub m: Mat,
    pub geo: String,
    pub dg: Option<Arc<Dg>>,
    pub leaves: Vec<Leaf>,
    pub car: Option<usize>,
    pub riders: Vec<Leaf>,
}

/// An elevator car placed by `place_cars`.
pub(super) struct Car {
    pub plan: Plan,
    pub leaves: Vec<Leaf>,
    pub id: usize,
    pub box_: Option<[V3; 2]>,
    pub panel: Option<V3>,
    pub n: usize,
}

/// Key of a tagged entity in `door_of` / the static-door set: (container instance, entity GUID) for container entities,
/// (LOADOUT_SCOPE, uid) for a loadout item, None when it has neither.
pub(super) fn door_key(tag: &Tag) -> Option<(usize, String)> {
    if let Some(g) = tag.guid.as_deref().filter(|g| !g.is_empty()) {
        return Some((tag.scope.unwrap_or(usize::MAX - 1), g.to_string()));
    }
    tag.uid.map(|u| (LOADOUT_SCOPE, u.to_string()))
}

/// (door item ids, number of linked doors without a readable animation) a panel reaches through `EntityLinks`, breadth first
/// (panel -> sensor -> door).  EMPIRICAL: door panels link to their door or its sensor; ship lift panels link to the lift `SCItemDoor`
/// entity.  The walk stays inside one container instance (`scope`).
fn linked_doors(
    scope: usize,
    guid: &str,
    first: &[String],
    door_of: &HashMap<(usize, String), usize>,
    stat: &HashSet<(usize, String)>,
    links: &HashMap<(usize, String), Vec<String>>,
) -> (Vec<usize>, usize) {
    let (mut out, mut nstatic) = (vec![], 0usize);
    let mut seen: HashSet<String> = HashSet::from([guid.to_string()]);
    let mut todo: std::collections::VecDeque<String> = first.iter().cloned().collect();
    while let Some(g) = todo.pop_front() {
        if !seen.insert(g.clone()) {
            continue;
        }
        let k = (scope, g.clone());
        if let Some(&id) = door_of.get(&k) {
            out.push(id);
        } else if stat.contains(&k) {
            nstatic += 1;
        } else if let Some(l) = links.get(&k) {
            todo.extend(l.iter().cloned());
        }
    }
    (out, nstatic)
}

fn r(v: f64, n: usize) -> Value {
    json!(rnd(v, n))
}

fn rv(v: &[f64], n: usize) -> Value {
    Value::Array(v.iter().map(|x| r(*x, n)).collect())
}

fn dist3(a: V3, b: V3) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn p_of(v: &Value) -> V3 {
    let a = v.as_array().map(Vec::as_slice).unwrap_or(&[]);
    [a.first().and_then(Value::as_f64).unwrap_or(0.0), a.get(1).and_then(Value::as_f64).unwrap_or(0.0), a.get(2).and_then(Value::as_f64).unwrap_or(0.0)]
}

/// `json.dumps(x, sort_keys=True)` as a comparison key.
fn canon(v: &Value) -> String {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            format!("{{{}}}", keys.iter().map(|k| format!("{}:{}", serde_json::to_string(k).unwrap(), canon(&o[*k]))).collect::<Vec<_>>().join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canon).collect::<Vec<_>>().join(",")),
        _ => v.to_string(),
    }
}

/// What the compile needs from the zone to build the feeds.
pub(super) struct Ctx<'c, 'a> {
    pub cat: &'c Catalog<'a>,
    pub src: &'c GeoSource<'c>,
    pub registry: &'c mut MatRegistry<'a>,
    pub own: &'c OwnMaterial<'c, 'c>,
    pub panel_style: &'c PanelStyle<'a>,
    pub hull: Option<&'c str>,
    pub title: Option<&'c str>,
    pub ship_cmds: &'c [Value],
    pub panel_geos: &'c mut HashMap<String, bool>,
}

/// The `models[]` entry (joints and groups, shared by every instance of a door / seat / turret model) of a split model.
fn model_of_dg(dg: &Dg) -> Value {
    dg.model_json()
}

/// The `turrets[]` feed entry of a split turret model (the viewer turns its joints): `j` = the turret joints (`station`),
/// `riders` = [{i: instance, g: joint}] items (guns) that hang on a joint, `bm` = the turret's placement matrix, `ext` = radius of its
/// meshes, `item` = the seat item of a manned turret (else -1).
fn turret_entry(tid: usize, ent: &Tagged, d: &Desc, dg: &Dg, model: usize, item: i64) -> Value {
    let spec = d.turret.as_ref();
    json!({"id": tid, "item": item, "cls": ent.tag.cls, "label": ent.tag.cls.replace('_', " "), "port": ent.tag.name.clone().unwrap_or_default(),
        "m": model, "j": dg.turret.as_ref().map(|t| t.iter().map(|j| j.to_json()).collect::<Vec<_>>()).unwrap_or_default(),
        "gyro": spec.map_or(true, |s| s.gyro), "fire": spec.and_then(|s| s.fire_angle.clone()), "recenter": spec.map_or(false, |s| s.recenter),
        "remote": spec.map_or(false, |s| s.remote), "bm": ent.m.iter().map(|v| rnd(*v, 5)).collect::<Vec<_>>(), "ext": dg.ext.unwrap_or(5.0),
        "riders": ent.riders.iter().map(Leaf::json).collect::<Vec<_>>()})
}

/// Add the station fields to a seat / turret feed item: `sub` (sub kind), `modes` (operator modes), `ctl`, `bt` (names of the seat
/// console's buttons) and `os` = index into `opsets` (the seat's own commands plus the ship's controller commands) with `grp` = the
/// command groups its operator modes make relevant (`station`).
fn station_fields(item: &mut Map<String, Value>, d: &Desc, ship_cmds: &[Value], opsets: &mut Vec<Value>, opset_of: &mut HashMap<String, usize>, tag: &Tag) {
    if !d.subkind.is_empty() {
        item.insert("sub".into(), json!(d.subkind));
    }
    if let Some(m) = &d.modes {
        item.insert("modes".into(), m.to_json());
    }
    if !d.ctl.is_empty() {
        item.insert("ctl".into(), json!(d.ctl));
    }
    if !d.btns.is_empty() {
        item.insert("bt".into(), Value::Array(d.btns.iter().map(|b| b["id"].clone()).collect()));
    }
    if !d.seat_type.is_empty() {
        item.insert("st".into(), json!(d.seat_type));
    }
    let mut cmds: Vec<Value> = d.cmds.clone();
    let have: HashSet<String> = cmds.iter().map(|c| c["id"].to_string()).collect();
    cmds.extend(ship_cmds.iter().filter(|c| !have.contains(&c["id"].to_string())).cloned());
    item.insert("grp".into(), json!(station::groups_for(d.modes.as_ref(), d.subkind)));
    if !cmds.is_empty() {
        let key = canon(&Value::Array(cmds.clone()));
        let idx = *opset_of.entry(key).or_insert_with(|| {
            opsets.push(Value::Array(cmds));
            opsets.len() - 1
        });
        item.insert("os".into(), json!(idx));
    }
    item.insert("port".into(), json!(tag.name.clone().unwrap_or_default()));
}

/// Give every remote-turret seat (sub kind 'remote_turret') the turrets it may control: `rt` = ids of the remote turrets of this ship /
/// zone, `rtd` = the default one, `rtl` = how it was chosen.  The data has no seat -> turret link (EMPIRICAL); the default is the
/// turret whose loadout port is named like the seat's port minus '_seat' ('name'), else the nearest remote turret not yet taken
/// ('nearest', INFERRED).
fn link_remote_seats(items: &mut [Value], turrets: &[Value]) {
    let remote: Vec<&Value> = turrets.iter().filter(|t| t["item"].as_i64().unwrap_or(-1) < 0).collect();
    if remote.is_empty() {
        return;
    }
    let bm_pos = |t: &Value| -> V3 {
        let b = t["bm"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        [b[3].as_f64().unwrap_or(0.0), b[7].as_f64().unwrap_or(0.0), b[11].as_f64().unwrap_or(0.0)]
    };
    let mut taken: HashSet<i64> = HashSet::new();
    let seats: Vec<usize> = items.iter().enumerate().filter(|(_, it)| it["sub"] == "remote_turret").map(|(i, _)| i).collect();
    for &si in &seats {
        let port = items[si]["port"].as_str().unwrap_or("").to_string();
        let want = port.strip_suffix("_seat").map(str::to_string);
        let pick = remote.iter().find(|t| want.as_deref().map_or(false, |w| t["port"] == w));
        if let Some(t) = pick {
            items[si]["rtl"] = json!("name");
            taken.insert(t["id"].as_i64().unwrap());
            items[si]["rtd"] = t["id"].clone();
        }
    }
    for &si in &seats {
        if items[si].get("rtd").is_some() {
            continue;
        }
        let p = p_of(&items[si]["p"]);
        let free: Vec<&&Value> = remote.iter().filter(|t| !taken.contains(&t["id"].as_i64().unwrap())).collect();
        let pool: Vec<&Value> = if free.is_empty() { remote.clone() } else { free.into_iter().copied().collect() };
        let mut best = pool[0];
        for t in &pool[1..] {
            if dist3(bm_pos(t), p) < dist3(bm_pos(best), p) {
                best = t;
            }
        }
        items[si]["rtd"] = best["id"].clone();
        items[si]["rtl"] = json!("nearest");
        taken.insert(best["id"].as_i64().unwrap());
    }
    for &si in &seats {
        let p = p_of(&items[si]["p"]);
        let mut sorted: Vec<&Value> = remote.clone();
        sorted.sort_by(|a, b| dist3(bm_pos(a), p).partial_cmp(&dist3(bm_pos(b), p)).unwrap());
        items[si]["rt"] = Value::Array(sorted.iter().map(|t| t["id"].clone()).collect());
    }
}

/// Does the panel model `geo` have a display: a render material that `mfd::rtt_kind` classes as 'screen' (a DisplayScreen with the
/// render-target texture; EMPIRICAL, the same test that picks the MFD screens)?  Also remembers the model so the geometry build marks
/// that screen for the viewer's panel clicks.  Generic: decided by the model's material, never by a class or ship name.  A model that
/// cannot be read has no display.
fn panel_display(ctx: &mut Ctx, geo: &str) -> bool {
    let key = mfd::geo_key(geo);
    if let Some(&h) = ctx.panel_geos.get(&key) {
        return h;
    }
    let opts = Options { max_render_tris: Some(super::ZONE_RENDER_TRIS), mtl_override: ctx.own.get(geo), with_frames: false, ..Options::default() };
    let has = match build_scene_named(ctx.src, geo, &opts, ctx.registry) {
        Ok((_info, parts)) => parts.iter().any(|pt| {
            pt.kind == "render" && pt.mat.map_or(false, |m| m >= 0 && ctx.registry.materials[m as usize].get("rtt").and_then(Value::as_str) == Some("screen"))
        }),
        Err(_) => false,
    };
    ctx.panel_geos.insert(key, has);
    has
}

/// Everything the interactables compile produces.
pub(super) struct InterOut {
    pub json: Vec<u8>,
    /// the feed items (door items are read by the elevator feed)
    pub items: Vec<Value>,
    pub notes: Vec<String>,
}

/// Build the `k=inter` feed from the tagged entities collected by the compile (see `interact.rs` for the data and approximations) and
/// return the notes for the info panel.
pub(super) fn compile_interactables(ctx: &mut Ctx, tagged: &[Tagged], ladders_in: &[LadderRec], links: &HashMap<(usize, String), Vec<String>>, static_doors: usize, static_guids: &HashSet<(usize, String)>) -> InterOut {
    let mut items: Vec<Value> = vec![];
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut ladders: Vec<Value> = vec![];
    let mut panels: Vec<&Tagged> = vec![];
    let mut door_of: HashMap<(usize, String), usize> = HashMap::new();
    let mut models: Vec<Value> = vec![];
    let mut model_of: HashMap<String, usize> = HashMap::new();
    let mut approx_doors = 0usize;
    let mut turrets: Vec<Value> = vec![];
    let mut opsets: Vec<Value> = vec![];
    let mut opset_of: HashMap<String, usize> = HashMap::new();
    let src = ctx.src;
    for ent in tagged {
        let (tag, d, m) = (&ent.tag, &*ent.tag.d, &ent.m);
        let nice = tag.cls.replace('_', " ");
        if d.kind == "aim" {
            // a remote-controlled turret (no seat): only its joints matter, it is driven from a remote station
            if let Some(dg) = ent.dg.as_ref().filter(|dg| dg.turret.is_some()) {
                let mkey = format!("aim|{}|{}", ent.geo, dg.turret.as_ref().unwrap().iter().map(|j| j.j.n.clone()).collect::<Vec<_>>().join("\u{1}"));
                let mi = *model_of.entry(mkey).or_insert_with(|| {
                    models.push(model_of_dg(dg));
                    models.len() - 1
                });
                let mut t = turret_entry(turrets.len(), ent, d, dg, mi, -1);
                t["leaves"] = Value::Array(ent.leaves.iter().map(Leaf::json).collect());
                turrets.push(t);
            }
            continue;
        }
        if d.kind == "panel" {
            panels.push(ent);
            continue;
        }
        if d.kind == "door" {
            let Some(dg) = ent.dg.as_ref() else { continue };
            let local = dg.ip.unwrap_or(dg.centre);
            let p = ivo::xform(m, local);
            let mi = *model_of.entry(ent.geo.clone()).or_insert_with(|| {
                models.push(model_of_dg(dg));
                models.len() - 1
            });
            approx_doors += dg.approx as usize;
            let spd = d.speed.max(0.05);
            let mut it = json!({"id": items.len(), "k": "door", "cls": tag.cls, "label": nice, "p": rv(&p, 3), "r": r(d.radius, 2), "open": d.open,
                "t": r(dg.seconds / spd, 3), "tc": r(dg.seconds_close / spd, 3), "m": mi, "leaves": ent.leaves.iter().map(Leaf::json).collect::<Vec<_>>()});
            if !ent.riders.is_empty() {
                // items hung on a helper of the door's moving nodes follow them in the viewer
                it["rd"] = Value::Array(ent.riders.iter().map(Leaf::json).collect());
            }
            if let (Some(bx), Some(sw)) = (&dg.box_, &dg.sweep) {
                // closed box of the moving parts and the box they sweep, zone coordinates (`liftpath`)
                let f = |b: [V3; 2]| Value::Array(b.iter().map(|c| rv(c, 2)).collect());
                it["bx"] = f(box_xf(m, bx));
                it["sw"] = f(box_xf(m, sw));
            }
            *counts.entry("door").or_insert(0) += 1;
            if let Some(k) = door_key(tag) {
                door_of.insert(k, items.len());
            }
            items.push(it);
            continue;
        }
        let dg = ent.dg.as_deref(); // an animated chair: seat_groups; shares the door's model / leaves mechanism
        let Some(sp) = ctx.cat.seat_point(src, &ent.geo, d, m, dg) else {
            *counts.entry("seat_unplaced").or_insert(0) += 1;
            continue;
        };
        let mut it = Map::new();
        it.insert("id".into(), json!(items.len()));
        it.insert("k".into(), json!(d.kind));
        it.insert("cls".into(), json!(tag.cls));
        it.insert("label".into(), json!(nice));
        it.insert("p".into(), rv(&sp.pos, 3));
        it.insert("r".into(), r(d.radius, 2));
        it.insert("eye".into(), rv(&sp.eye, 3));
        it.insert("yaw".into(), r(sp.yaw, 4));
        it.insert("exit".into(), rv(&sp.exit, 3));
        it.insert("lim".into(), json!({"yaw": d.yaw, "pitch": d.pitch}));
        if let Some(y) = sp.exit_yaw {
            // the game's exit helper has an orientation of its own (the viewer picks the exit by it)
            it.insert("xyaw".into(), r(y, 4));
        }
        if let Some(b) = &d.bed {
            // a bed: Sit down and / or Lie down (interact.rs bed_info); the lying pose has its own eye / view
            it.insert("bed".into(), json!({"modes": b.modes, "main": b.main}));
            if let Some(lie) = sp.lie.as_ref().filter(|l| !l.is_null()) {
                it.insert("lie".into(), bed::rounded(lie));
            }
            *counts.entry("bed").or_insert(0) += 1;
        }
        let mut turret_here = false;
        if let Some(dg) = dg {
            if let (Some(eyem), Some(eyeg)) = (sp.eyem, sp.eyeg) {
                it.insert("eyem".into(), rv(&eyem, 4));
                it.insert("eyeg".into(), json!(eyeg));
            }
            // the same chair model has other clips per seat class (30 / 50 cm)
            let mkey = format!(
                "seat|{}|{}|{}|{}",
                ent.geo,
                d.adb,
                d.mtag,
                dg.turret.as_ref().map(|t| t.iter().map(|j| j.j.n.clone()).collect::<Vec<_>>().join("\u{1}")).unwrap_or_default()
            );
            let mi = *model_of.entry(mkey).or_insert_with(|| {
                models.push(model_of_dg(dg));
                models.len() - 1
            });
            it.insert("m".into(), json!(mi));
            it.insert("t".into(), r(dg.seconds, 3));
            it.insert("tc".into(), r(dg.seconds_close, 3));
            it.insert("leaves".into(), Value::Array(ent.leaves.iter().map(Leaf::json).collect()));
            *counts.entry("seat_anim").or_insert(0) += 1;
            if !ent.riders.is_empty() && dg.turret.is_none() {
                // (a turret's riders travel in its turrets[] entry)
                it.insert("rd".into(), Value::Array(ent.riders.iter().map(Leaf::json).collect()));
            }
            if dg.turret.is_some() {
                // a manned turret: the seat item carries the animated parts, the turret entry the joints (limits, speed, riders)
                let te = turret_entry(turrets.len(), ent, d, dg, mi, items.len() as i64);
                it.insert("tu".into(), te["id"].clone());
                turrets.push(te);
                turret_here = true;
            }
        }
        let _ = turret_here;
        station_fields(&mut it, d, ctx.ship_cmds, &mut opsets, &mut opset_of, tag);
        *counts.entry(d.kind).or_insert(0) += 1;
        items.push(Value::Object(it));
    }
    // ladders, also the ones without a model of their own (EMPIRICAL: 358 of 888)
    for ld in ladders_in {
        let (mut bx, mut ge) = (None, None);
        if let Some(g) = ld.geo.as_deref().filter(|g| !g.is_empty()) {
            ge = ctx.cat.archive_name(g);
            bx = ge.as_deref().and_then(|n| ctx.cat.model_box(n));
        }
        if let Some(li) = ladder_item(&ld.d, &ld.params, &ld.m, bx) {
            let mut o = Map::new();
            o.insert("id".into(), json!(ladders.len()));
            o.insert("cls".into(), json!(ld.cls));
            o.insert("label".into(), json!(ld.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Ladder".into())));
            o.insert("geo".into(), json!(ge.unwrap_or_default()));
            for (k, v) in li {
                o.insert(k, v);
            }
            ladders.push(Value::Object(o));
        }
    }
    // ---- panels (resolved after the doors)
    let n_panels = compile_panels(ctx, &panels, &mut items, &door_of, links, static_guids);
    counts.insert("panel", n_panels);
    let lifts: Vec<i64> = {
        let mut v: Vec<i64> = vec![];
        for it in items.iter().filter(|it| it["k"] == "panel" && it["sub"] == "elevator") {
            for d in it["doors"].as_array().unwrap() {
                v.push(d.as_i64().unwrap());
            }
        }
        v.sort();
        v.dedup();
        v
    };
    let lift_path = liftpath::link_lift_paths(&mut items, &lifts);
    let _ = lift_path;
    link_remote_seats(&mut items, &turrets);
    let json = serde_json::to_vec(&json!({"items": items, "models": models, "ladders": ladders, "turrets": turrets, "opsets": opsets,
        "approx": if approx_doors > 0 { APPROX_NOTE } else { "" }}))
    .unwrap();
    let c = |k: &str| counts.get(k).copied().unwrap_or(0);
    let mut notes = vec![];
    if c("panel") > 0 {
        let near = items.iter().filter(|it| it["k"] == "panel" && it["link"] == "nearest").count();
        notes.push(format!(
            "{} wall panels (door / light / elevator controls) get an HTML panel instead of the game's Scaleform screen{}",
            c("panel"),
            if near > 0 { format!("; {} door panels have no entity link and use the nearest door (inferred)", near) } else { String::new() }
        ));
    }
    if !ladders.is_empty() {
        notes.push(format!(
            "{} ladders (F on a ladder to climb it; climb axis, height and top side from the Ladder entity, the side you climb from is chosen from the model)",
            ladders.len()
        ));
    }
    if !items.is_empty() || static_doors > 0 {
        notes.push(format!(
            "interactions (hold F in first person): {} doors, {} seats, {} turret seats{}{}; {}",
            c("door"),
            c("seat"),
            c("turret"),
            if c("seat_unplaced") > 0 { format!(", {} seats without a sit helper in their model", c("seat_unplaced")) } else { String::new() },
            if static_doors > 0 { format!(", {} doors without a readable animation stay shut", static_doors) } else { String::new() },
            format!(
                "door motion from the game's animation keys (.dba){}",
                if approx_doors > 0 { format!("; {} doors have no decodable key track and move approximately", approx_doors) } else { String::new() }
            )
        ));
    }
    if c("seat_anim") > 0 {
        notes.push(format!(
            "{} seats have a chair part that moves when you sit down and stand up (the sit / stand clips of the seat's animation database; the clip is picked by the seat's mannequin tags and the timing is not the engine's)",
            c("seat_anim")
        ));
    }
    InterOut { json, items, notes }
}

/// Append the wall panels (kind 'panel', see `Catalog::describe`) to the feed; returns how many.  A door panel gets `doors` = ids of the
/// door items it controls: those reached through the entity's `EntityLinks` (directly or through a sensor) with `link` 'data'; a panel
/// whose links reach no door (or that has none) takes the nearest door within PANEL_NEAREST metres with `link` 'nearest' (INFERRED, not
/// data).  Elevator panels follow the same links: on ships the lift is an `SCItemDoor` entity that the panel links to; without a link
/// they may take the nearest door whose class names a lift (LIFT_DOOR, INFERRED).  A panel that is a loadout item (no EntityLinks at
/// all) belongs to the item port it hangs on: its parent item, when that is a door / lift (EMPIRICAL, AEGS Hammerhead family).
fn compile_panels(ctx: &mut Ctx, panels: &[&Tagged], items: &mut Vec<Value>, door_of: &HashMap<(usize, String), usize>, links: &HashMap<(usize, String), Vec<String>>, stat: &HashSet<(usize, String)>) -> usize {
    let doors: Vec<(usize, String, V3)> = items.iter().filter(|it| it["k"] == "door").map(|it| (it["id"].as_u64().unwrap() as usize, it["cls"].as_str().unwrap_or("").to_string(), p_of(&it["p"]))).collect();
    let mut n = 0;
    let src = ctx.src;
    for ent in panels {
        let (tag, d, m) = (&ent.tag, &*ent.tag.d, &ent.m);
        let p = ctx.cat.panel_point(src, &ent.geo, m);
        let mut item = Map::new();
        item.insert("id".into(), json!(items.len()));
        item.insert("k".into(), json!("panel"));
        item.insert("sub".into(), json!(d.sub));
        item.insert("cls".into(), json!(tag.cls));
        item.insert("label".into(), json!(tag.cls.replace('_', " ")));
        item.insert("p".into(), rv(&p, 3));
        item.insert("r".into(), r(d.radius, 2));
        item.insert("ops".into(), json!(d.ops));
        item.insert("primary".into(), json!(d.primary));
        item.insert("link".into(), json!("none"));
        item.insert("doors".into(), json!([]));
        if LINK_SUBS.contains(&d.sub) {
            let (ds, ns) = if let Some(uid) = tag.uid {
                // a loadout item has no EntityLinks: the item it hangs on is what it belongs to (see door_key)
                let first: Vec<String> = tag.parent.map(|p| vec![p.to_string()]).unwrap_or_default();
                linked_doors(LOADOUT_SCOPE, &uid.to_string(), &first, door_of, stat, &HashMap::new())
            } else {
                linked_doors(tag.scope.unwrap_or(usize::MAX - 1), tag.guid.as_deref().unwrap_or(""), &tag.links, door_of, stat, links)
            };
            item.insert("doors".into(), json!(ds));
            if ns > 0 {
                item.insert("static".into(), json!(ns));
            }
            if !ds.is_empty() || ns > 0 {
                item.insert("link".into(), json!("data"));
            } else {
                // no links, or links that reach no door of the feed (EMPIRICAL Tiburon: a link ends at an entity that is no door); an
                // elevator panel only guesses among doors whose class names a lift, a door panel among all
                let mut near: Option<&(usize, String, V3)> = None;
                for dd in doors.iter().filter(|dd| d.sub == "door" || lift_door().is_match(&dd.1)) {
                    if near.map_or(true, |b| dist3(dd.2, p) < dist3(b.2, p)) {
                        near = Some(dd);
                    }
                }
                if let Some(nr) = near {
                    if dist3(nr.2, p) <= if d.sub == "door" { PANEL_NEAREST } else { LIFT_NEAREST } {
                        item.insert("doors".into(), json!([nr.0]));
                        item.insert("link".into(), json!("nearest"));
                    }
                }
            }
        }
        // manufacturer (and accent colour from the game's UI assets) of the panel: its own model path, else the ship's hull / name
        let (mfr, acc) = ctx.panel_style.of(&[&tag.cls, &ent.geo, ctx.hull.unwrap_or(""), ctx.title.unwrap_or("")]);
        item.insert("mfr".into(), json!(mfr));
        item.insert("acc".into(), json!(acc));
        if LINK_SUBS.contains(&d.sub) {
            // does the model carry a display (a render-target screen material)? the viewer draws on it
            let has = panel_display(ctx, &ent.geo);
            item.insert("disp".into(), json!(has as i32));
        }
        items.push(Value::Object(item));
        n += 1;
    }
    n
}

/// Build the `k=elev` feed once the instance indices are known; returns the document and the info notes.
pub(super) fn compile_elevators(items: &[Value], tagged: &[Tagged], cars: &[Car]) -> (Option<Vec<u8>>, Vec<String>) {
    if cars.is_empty() {
        return (None, vec![]);
    }
    let door_items: Vec<&Value> = items.iter().filter(|it| it["k"] == "door").collect();
    let mut door_leaves: HashMap<usize, HashSet<usize>> = HashMap::new();
    for ent in tagged {
        if let Some(c) = ent.car {
            door_leaves.entry(c).or_default().extend(ent.leaves.iter().map(|l| l.i));
        }
    }
    let (mut feed, mut approx) = (vec![], 0usize);
    let empty = HashSet::new();
    for car in cars {
        let pl = &car.plan;
        let mine = door_leaves.get(&car.id).unwrap_or(&empty);
        let panel = elevator::car_panel_local(car.panel);
        let mut leaves: Vec<usize> = car.leaves.iter().map(|l| l.i).collect();
        leaves.sort();
        leaves.dedup();
        let doors: Vec<Value> = door_items
            .iter()
            .filter(|it| it["leaves"].as_array().map_or(false, |ls| ls.iter().any(|lf| mine.contains(&(lf["i"].as_u64().unwrap() as usize)))))
            .map(|it| it["id"].clone())
            .collect();
        feed.push(json!({"id": car.id, "name": pl.name, "cab": pl.cab.rsplit('/').next().unwrap(), "source": pl.source, "at": pl.at,
            "leaves": leaves, "doors": doors, "stops": pl.stops.iter().map(|s| s.to_json()).collect::<Vec<_>>(), "legs": pl.legs_json(),
            "box": car.box_, "panel": panel, "accel": pl.accel, "speed": pl.speed, "slide": pl.slide}));
        approx += (pl.source == "tags") as usize;
    }
    let doc = serde_json::to_vec(&json!({"elevators": feed})).unwrap();
    let note = format!(
        "{} elevators ({} car parts): the car container is drawn at its first stop and travels a straight line between stops when used (F at a call panel or at the panel inside the car); the trip's shape and timing are approximate{}",
        cars.len(),
        cars.iter().map(|c| c.n).sum::<usize>(),
        if approx > 0 { format!("; {} of them have their manager outside this zone and were found by matching tags and destination names", approx) } else { String::new() }
    );
    (Some(doc), vec![note])
}
