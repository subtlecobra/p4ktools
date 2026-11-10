//! The sounds that belong to a ship: which audio trigger each part of it plays, when, and which Wwise events / media that is.
//!
//! Where the links come from (all read from data, nothing hard-coded per ship):
//!
//! * DataCore entity records.  Every `GlobalResourceAudio` value (`audioTrigger` = an ATL trigger name) found anywhere in the components
//!   of the ship, of the items of its default loadout and of the entity classes in its object containers.  CONFIRMED structure (live
//!   archive): `ShipAudioComponentParams`, `ThrusterAudioComponentParams`, `QuantumDriveAudioParams`, `SCItemWeaponComponentParams` /
//!   `SCItemTurretParams`, `TransitCarriageParams.audio` and `SEntityComponentEffects.entityEffects.soundEffects` (tag / trigger
//!   effects: a sound played while a game tag is set or when a trigger fires), `SAttachableComponentParams.audioAttachParams`,
//!   `SActorUsableParams` transition triggers.
//! * Per-entity overrides in the object containers (`audioTrigger="..."` attributes of AudioTriggerSpot, AudioAreaAmbience,
//!   VehicleAudioPoint ...), read by formats/socpak (`audio` of an entity).  These carry the entity position.
//! * ATL (atl.rs): trigger name -> Wwise event name, loop or one-shot, attenuation radius, bank, game parameters it reacts to.
//! * Soundbanks (sounds.rs): event id = FNV-1 of the event name -> media ids.
//!
//! What is NOT in the data and therefore INFERRED: *when* the engine fires a trigger.  The condition text is derived from the field name
//! that holds the trigger (`shipPowerOnAudioTrigger` -> "ship power on") by the table `RULES`.  Which gameplay value feeds an RTPC is
//! only known by its name.
//!
//! The ship's own geometry/loadout placement (`ships::loadout_items`) belongs to the ship layer; it is injected through `LoadoutSource`.
//! The entries are `serde_json` maps with a fixed key order, so the JSON the Sounds tab fetches is stable.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use p4k_formats::dcb::Value as Dv;
use p4k_formats::socpak::{self, compose, quat_scale_pos_matrix};
use p4k::P4k;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::atl::Atl;
use crate::itemmaterials::ItemMaterials;
use crate::sounds::SoundData;

/// (regex on "<component>.<field path>" lower-cased, system, condition text).  First match wins.  INFERRED from the field names.
const RULES: &[(&str, &str, &str)] = &[
    ("shippoweron", "Ship power", "ship powered on (power button / master power)"),
    ("shippoweroff", "Ship power", "ship powered off"),
    ("mastermodespooling(started|finished)", "Flight modes", "master mode (SCM / NAV) switch: spool-up started / finished"),
    ("landinggear.*extend", "Landing gear", "landing gear extends"),
    ("landinggear.*retract", "Landing gear", "landing gear retracts"),
    ("afterburner.*(enabled|start)", "Engines & thrusters", "afterburner engaged (boost key)"),
    ("afterburner.*(disabled|stop)", "Engines & thrusters", "afterburner released"),
    ("(thrusterparams|thrusteraudio).*(startloop|looprstart|thrusterloopstart|spoolstart)", "Engines & thrusters", "loop while the thruster is running; its strength follows the thruster RTPCs"),
    ("(thrusterparams|thrusteraudio).*(stoploop|thrusterloopstop|spoolstop)", "Engines & thrusters", "ends the thruster loop"),
    ("(startoneshot|thrusterononeshot|spoolupstarted|spoolupcompleted)", "Engines & thrusters", "thruster / engine switched on (spool up)"),
    ("(stoponeshot|thrusteroffoneshot|spooldownstarted|spooldowncompleted)", "Engines & thrusters", "thruster / engine switched off (spool down)"),
    ("misfire", "Engines & thrusters", "thruster misfire (damaged / overheated thruster)"),
    ("takeoff", "Flight", "ship lifts off the ground"),
    ("landing(oneshot|sweetener)|landedsweetener", "Flight", "ship touches down"),
    ("contactsweetener|collision", "Damage & collision", "hull contact / collision"),
    ("passby|movementloop|shipmovement|trackview", "Flight", "continuous movement / pass-by sound for other players; driven by speed RTPCs"),
    ("jerk", "Flight", "strong linear / angular acceleration"),
    ("shipwideaudioobjects", "Ambience", "continuous cockpit / ship-wide loop while the ship is running (RTPC driven)"),
    ("quantumdrive.*(spool|align|rampup|flightinprogress|rampdown|cooldown|ui)|quantum.*(spool|align|calibrat)", "Quantum drive", "quantum drive state (spool, align, ramp, travel, cool down)"),
    ("quantumboost|jumpdrive", "Quantum drive", "quantum boost / jump drive state"),
    ("dockinganimator|dockingtube|fuelnozzle", "Docking & refuel", "docking arm / tube / refuel event"),
    ("turret.*(rotate|rotation)|scentityweaponmount", "Turrets", "turret rotating (yaw / pitch) or mount used"),
    ("weapon|ordnance|missile|bomb|firea|report", "Weapons", "weapon fired / charged / dry-fired / overheated"),
    ("transitcarriage|transportcarriage|elevator", "Elevators", "lift moves / arrives (carriage state)"),
    ("door|navigationlink|transitionaudiotrigger", "Doors", "door opens / closes (usable transition)"),
    ("seat|sactorusable", "Seats & interaction", "seat / usable entered or exited"),
    ("audioareaambience|audiotriggerspot|vehicleaudiopoint|roomtone", "Ambience", "continuous ambience around this position"),
    ("light(group|audio)|flashlight", "Lights", "light switched on / off"),
    ("salvage|mining|tractor|beam", "Tools", "tool beam / salvage / mining"),
    ("health|destr|break|explosion", "Damage & collision", "part destroyed / exploded"),
    ("videoaudio|jukebox|commsspeaker", "Screens & media", "screen / media device plays or pauses its video audio"),
    ("attach", "Items", "item installed into / removed from its port"),
    ("physicalaudio|pickup|drop|place", "Items", "item handled (pick up, drop, place, slide)"),
];

pub const ORDER: &[&str] = &[
    "Ship power", "Flight modes", "Engines & thrusters", "Flight", "Quantum drive", "Landing gear", "Doors", "Elevators", "Seats & interaction", "Weapons",
    "Turrets", "Ambience", "Damage & collision", "Docking & refuel", "Screens & media", "Lights", "Tools", "Items", "Other",
];

/// EMPIRICAL (Cutlass, Javelin, Perseus, Freelancer, Polaris: all carry the same SEntityComponentEffects): game tags QDriveSpooling /
/// QDriveTravelingEffect / QDriveInterdicting and triggers QDriveEntryFlash / QDriveExitFlash / Flight_System_Power_On / _Off.
const CLASS_RULES: &[(&str, &str, &str)] = &[
    ("q ?drive|qdrive|quantum|qtravel|interdict", "Quantum drive", "quantum drive stage (tag / trigger effect of the ship)"),
    ("flight[ _]system[ _]power|control power", "Ship power", "ship systems powered on / off (trigger effect of the ship)"),
    ("explosion|\\bdeath\\b", "Damage & collision", "ship destroyed (trigger effect)"),
    ("elevator|lift|platform|cargolift", "Elevators", "lift ramps up / moves / stops"),
    ("landing|gear", "Landing gear", "landing gear moves"),
    ("door|hatch|ramp|airlock|gate", "Doors", "door / hatch / ramp / airlock state"),
    ("seat|bed|chair", "Seats & interaction", "seat or bed used"),
    ("locker|cabinet|drawer|storage|cargo", "Items", "storage opened / closed"),
];

struct Compiled {
    rules: Vec<(Regex, &'static str, &'static str)>,
    class_rules: Vec<(Regex, &'static str, &'static str)>,
    humanize_tail: Regex,
    humanize_camel: Regex,
}

fn compiled() -> &'static Compiled {
    static C: std::sync::OnceLock<Compiled> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let comp = |t: &[(&'static str, &'static str, &'static str)]| t.iter().map(|&(r, s, c)| (Regex::new(&format!("(?i){}", r)).unwrap(), s, c)).collect();
        Compiled {
            rules: comp(RULES),
            class_rules: comp(CLASS_RULES),
            humanize_tail: Regex::new(r"(?i)(audio)?(trigger|oneshot|one_shot)$").unwrap(),
            humanize_camel: Regex::new(r"([a-z])([A-Z])").unwrap(),
        }
    })
}

/// (system, condition).  The component + field decides first; the generic ones (entity effects) fall back on the class name and the
/// effect / tag label, which is where a lift's 'In Transit' or a locker's 'Open' says what it is (INFERRED from the names).
pub fn classify(comp: &str, path: &str, cls: &str, label: &str) -> (&'static str, &'static str) {
    let c = compiled();
    let key = format!("{}.{}", comp, path).to_lowercase();
    for (rx, s, cond) in &c.rules {
        if rx.is_match(&key) {
            return (s, cond);
        }
    }
    let t = format!("{} {}", cls, label);
    for (rx, s, cond) in &c.class_rules {
        if rx.is_match(&t) {
            return (s, cond);
        }
    }
    ("Other", "(condition not known: see the field name)")
}

/// `shipPowerOnAudioTrigger` -> `ship power on`.
pub fn humanize(field: &str) -> String {
    let c = compiled();
    let last = field.rsplit('.').next().unwrap_or(field);
    let last = last.split('[').next().unwrap_or(last);
    let f = c.humanize_tail.replace(last, "");
    let f = c.humanize_camel.replace_all(&f, "${1} ${2}").replace('_', " ").to_lowercase();
    let f = f.trim();
    if f.is_empty() {
        field.to_string()
    } else {
        f.to_string()
    }
}

/// `loop` / `stop` / `oneshot` from the name prefix and the ATL duration type.
pub fn kind_of(trigger: &str, dur: Option<&str>) -> &'static str {
    let low = trigger.to_lowercase();
    if low.starts_with("stop_") || low.starts_with("pause_") {
        "stop"
    } else if dur == Some("Infinite") {
        "loop"
    } else {
        "oneshot"
    }
}

/// One audio trigger of an entity class record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hit {
    pub comp: String,
    pub path: String,
    pub trigger: String,
    pub effect: Option<String>,
    pub tag: Option<String>,
    pub mode: Option<&'static str>,
    pub params: Vec<String>,
}

/// A placed item of a ship's default loadout (what `Ships::loadout_items` yields): item class, port name, 3x4 matrix.
#[derive(Debug, Clone, Default)]
pub struct PlacedItem {
    pub cls: Option<String>,
    pub port: Option<String>,
    pub matrix: Option<[f64; 12]>,
}

/// The ship layer's default-loadout placement (`ships::loadout_items`), injected so this module does not depend on it.
pub trait LoadoutSource {
    fn loadout_items(&self, cls: &str) -> Vec<PlacedItem>;
}

fn truthy_str(v: Option<&Dv>) -> Option<&str> {
    v.and_then(Dv::as_str).filter(|s| !s.is_empty())
}

fn is_tuple(v: &Dv) -> bool {
    matches!(v, Dv::Strong(..) | Dv::Weak(..) | Dv::Ref(..) | Dv::ClassRef(..))
}

/// `round(x, 3)` (correctly rounded, half to even on the exact binary value, as Python's `round`).
pub fn round3(x: f64) -> f64 {
    format!("{:.3}", x).parse().unwrap_or(x)
}

/// 3x4 row-major matrix of an Offset: position and Ang3 rotation in degrees (Rz * Ry * Rx) (ships.offset_matrix).
pub fn offset_matrix(pos: [f64; 3], rot_deg: [f64; 3]) -> [f64; 12] {
    fn mul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
        [
            a[0] * b[0] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
            a[0] * b[1] + a[1] * b[0] + a[2] * b[3] - a[3] * b[2],
            a[0] * b[2] - a[1] * b[3] + a[2] * b[0] + a[3] * b[1],
            a[0] * b[3] + a[1] * b[2] - a[2] * b[1] + a[3] * b[0],
        ]
    }
    let deg = std::f64::consts::PI / 180.0;
    let h: Vec<f64> = rot_deg.iter().map(|a| a * deg / 2.0).collect();
    let qx = [h[0].cos(), h[0].sin(), 0.0, 0.0];
    let qy = [h[1].cos(), 0.0, h[1].sin(), 0.0];
    let qz = [h[2].cos(), 0.0, 0.0, h[2].sin()];
    quat_scale_pos_matrix(mul(qz, mul(qy, qx)), [1.0, 1.0, 1.0], pos)
}

/// Archive lookup key of a path written in a record ('objectcontainers\\ships\\...' or 'ObjectContainers/...') (ships.data_key).
pub fn data_key(path: &str) -> String {
    let p = path.replace('\\', "/").to_lowercase();
    let p = p.trim_start_matches('/');
    if p.starts_with("data/") {
        p.to_string()
    } else {
        format!("data/{}", p)
    }
}

#[derive(Debug, Clone)]
pub struct ContainerRef {
    pub file: String,
    pub pos: [f64; 3],
    pub rot: [f64; 3],
}

fn vec3(v: Option<&Dv>) -> [f64; 3] {
    let g = |k: &str| v.and_then(|o| o.get(k)).and_then(Dv::as_f64).unwrap_or(0.0);
    [g("x"), g("y"), g("z")]
}

const RECORD: &str = "EntityClassDefinition.";

/// Object containers of a ship class (ships.ship_record), or None when the class has no vehicle component.
pub fn ship_containers(items: &ItemMaterials, cls: &str) -> Option<Vec<ContainerRef>> {
    let d = items.datacore()?;
    let ids = d.record_index().get(&format!("{}{}", RECORD, cls))?;
    let rec = d.record(*ids.first()?).ok()?;
    let data = d.record_data(&rec, 8).ok()?;
    for comp in data.get("Components").and_then(Dv::as_list).unwrap_or(&[]) {
        if comp.as_obj().map_or(false, |o| o.struct_name == "VehicleComponentParams") {
            let mut out = Vec::new();
            for o in comp.get("objectContainers").and_then(Dv::as_list).unwrap_or(&[]) {
                let Some(file) = o.get("fileName").and_then(Dv::as_str) else { continue };
                let off = o.get("Offset");
                out.push(ContainerRef { file: file.to_string(), pos: vec3(off.and_then(|x| x.get("Position"))), rot: vec3(off.and_then(|x| x.get("Rotation"))) });
            }
            return Some(out);
        }
    }
    None
}

/// Everything the Sounds tab needs besides the request: the DataCore reader, the ATL index, the sound index (None while it builds).
pub struct ShipSounds<'a> {
    pub items: &'a ItemMaterials,
    pub atl: &'a Atl,
    pub sounds: Option<&'a SoundData>,
    hits: Mutex<HashMap<String, Arc<Vec<Hit>>>>,
}

pub type Entry = Map<String, Value>;

impl<'a> ShipSounds<'a> {
    pub fn new(items: &'a ItemMaterials, atl: &'a Atl, sounds: Option<&'a SoundData>) -> Self {
        ShipSounds { items, atl, sounds, hits: Mutex::new(HashMap::new()) }
    }

    fn tag_name(&self, r: Option<&Dv>) -> Option<String> {
        let Some(Dv::Ref(_, guid)) = r else { return None };
        let i = self.items.guid_record(guid)?;
        let d = self.items.datacore()?;
        let rec = d.record(i).ok()?;
        let data = d.record_data(&rec, 1).ok();
        truthy_str(data.as_ref().and_then(|v| v.get("tagName"))).map(str::to_string).or(Some(rec.name))
    }

    /// Every audio trigger of an entity class record (cached).
    pub fn class_hits(&self, cls: &str) -> Arc<Vec<Hit>> {
        if let Some(h) = self.hits.lock().unwrap().get(cls) {
            return h.clone();
        }
        let mut hits = Vec::new();
        if let Some(d) = self.items.datacore() {
            if let Some(ids) = d.record_index().get(&format!("{}{}", RECORD, cls)) {
                if let Some(data) = d.record(ids[0]).ok().and_then(|r| d.record_data(&r, 10).ok()) {
                    for c in data.get("Components").and_then(Dv::as_list).unwrap_or(&[]) {
                        let Some(co) = c.as_obj() else { continue };
                        let mut params: Vec<String> = Vec::new();
                        let n0 = hits.len();
                        self.walk(c, "", &co.struct_name, &mut hits, &mut params);
                        let mut uniq: Vec<String> = Vec::new();
                        for p in params {
                            if !uniq.contains(&p) {
                                uniq.push(p);
                            }
                        }
                        uniq.truncate(12);
                        for h in &mut hits[n0..] {
                            if h.params.is_empty() {
                                h.params = uniq.clone();
                            }
                        }
                    }
                }
            }
        }
        let arc = Arc::new(hits);
        self.hits.lock().unwrap().insert(cls.to_string(), arc.clone());
        arc
    }

    fn walk(&self, o: &Dv, path: &str, comp: &str, hits: &mut Vec<Hit>, params: &mut Vec<String>) {
        match o {
            Dv::Obj(ob) => {
                let st = ob.struct_name.as_str();
                if st == "GlobalResourceAudio" {
                    if let Some(t) = truthy_str(ob.get("audioTrigger")) {
                        hits.push(Hit { comp: comp.to_string(), path: path.to_string(), trigger: t.to_string(), ..Default::default() });
                    }
                    return;
                }
                if st == "SEntityEffectSystem_SoundTagEffect" || st == "SEntityEffectSystem_SoundTriggerEffect" {
                    if let Some(tg) = truthy_str(ob.get("soundEffect").and_then(|s| s.get("audioTrigger"))) {
                        let link = ob.get("tag").filter(|v| !matches!(v, Dv::Null)).or_else(|| ob.get("trigger"));
                        let mut ps = Vec::new();
                        for pl in ob.get("propertyLinks").and_then(Dv::as_list).unwrap_or(&[]) {
                            if let Some(r) = truthy_str(pl.get("rtpc").and_then(|x| x.get("rtpc"))) {
                                ps.push(r.to_string());
                            }
                        }
                        hits.push(Hit {
                            comp: comp.to_string(),
                            path: path.to_string(),
                            trigger: tg.to_string(),
                            effect: truthy_str(ob.get("name")).map(str::to_string),
                            tag: self.tag_name(link),
                            mode: Some(if st.ends_with("TagEffect") { "tag" } else { "trigger" }),
                            params: ps,
                        });
                    }
                    return;
                }
                if st == "AudioRtpc" {
                    if let Some(r) = truthy_str(ob.get("rtpc")) {
                        params.push(r.to_string());
                    }
                } else if st == "AudioSwitch" {
                    if let Some(r) = truthy_str(ob.get("switch")) {
                        params.push(format!("switch {}", r));
                    }
                }
                for (k, v) in &ob.fields {
                    let np = if path.is_empty() { k.clone() } else { format!("{}.{}", path, k) };
                    self.walk(v, &np, comp, hits, params);
                }
            }
            Dv::List(l) => {
                let np = format!("{}[]", path);
                for v in l {
                    if !is_tuple(v) {
                        self.walk(v, &np, comp, hits, params);
                    }
                }
            }
            _ => {}
        }
    }

    /// `{trigger, event, dur, radius, banks, rtpcs, event_id, media, reason, silent}` for a trigger name (empty values included; the
    /// callers drop `None` / `[]` / `""`).
    pub fn resolve(&self, trigger: &str) -> Entry {
        let mut out = Entry::new();
        out.insert("trigger".into(), json!(trigger));
        let Some(t) = self.atl.trigger(trigger) else {
            // CONFIRMED against the engine: it looks the lower-case CRC32 of the trigger name up in the
            // ATL trigger map and, when it is missing, logs "Could not find ATL Trigger named" and returns id 0: nothing plays.  There is
            // no fallback to an event of the same name, so such a trigger is silent in the game.
            out.insert("reason".into(), json!("trigger not defined in the ATL files: silent in the game or defined elsewhere"));
            out.insert("silent".into(), json!(true));
            out.insert("event".into(), json!(trigger));
            out.insert("event_id".into(), json!(p4k_formats::bnk::fnv1(trigger)));
            return out;
        };
        let ev = t.event.as_deref().filter(|e| !e.is_empty()).unwrap_or(trigger);
        out.insert("event".into(), json!(ev));
        out.insert("dur".into(), json!(t.dur));
        out.insert("radius".into(), json!(t.rmax));
        out.insert("banks".into(), json!(self.atl.banks(t)));
        out.insert("rtpcs".into(), json!(self.atl.specific_rtpcs(t)));
        let eid = p4k_formats::bnk::fnv1(ev);
        out.insert("event_id".into(), json!(eid));
        if let Some(d) = self.sounds {
            if d.events.contains_key(&eid) {
                let media = d.event_media.get(&eid).cloned().unwrap_or_default();
                if media.is_empty() {
                    out.insert("reason".into(), json!("event found but it plays no media (stop / state / bus action only)"));
                }
                out.insert("media".into(), json!(media));
            } else {
                out.insert("reason".into(), json!("no event with this name in the soundbanks"));
            }
        }
        out
    }

    /// Entries for one entity class (+ optional per-entity `extra` overrides [(path, trigger)] and a position).
    pub fn entity_hits(&self, cls: &str, where_: Option<[f64; 3]>, name: Option<&str>, extra: &[(String, String)], with_class: bool) -> Vec<Entry> {
        let mut rows: Vec<(Hit, &str)> = Vec::new();
        if with_class {
            rows.extend(self.class_hits(cls).iter().map(|h| (h.clone(), "class")));
        }
        for (path, trig) in extra {
            rows.push((Hit { comp: cls.to_string(), path: path.clone(), trigger: trig.clone(), ..Default::default() }, "entity"));
        }
        let mut out = Vec::new();
        for (h, source) in rows {
            let (system, cond) = classify(&h.comp, &h.path, cls, h.effect.as_deref().unwrap_or(""));
            let r = self.resolve(&h.trigger);
            let dur = r.get("dur").and_then(Value::as_str);
            let mut e = Entry::new();
            e.insert("system".into(), json!(system));
            e.insert("class".into(), json!(cls));
            e.insert("entity".into(), json!(name));
            e.insert("component".into(), json!(h.comp));
            e.insert("field".into(), json!(h.path));
            e.insert("what".into(), json!(humanize(&h.path)));
            e.insert("when".into(), json!(cond));
            e.insert("source".into(), json!(source));
            e.insert("kind".into(), json!(kind_of(&h.trigger, dur)));
            e.insert("params".into(), json!(h.params));
            if let Some(eff) = h.effect.as_deref().filter(|s| !s.is_empty()) {
                e.insert("what".into(), json!(eff));
                e.insert(
                    "when".into(),
                    json!(format!(
                        "{} '{}' ({} tag effect)",
                        if h.mode == Some("tag") { "while tag" } else { "when trigger" },
                        h.tag.as_deref().unwrap_or("None"),
                        h.mode.unwrap_or("None")
                    )),
                );
            }
            if let Some(w) = where_ {
                e.insert("pos".into(), json!(w));
            }
            if let Some(m) = h.mode {
                // CONFIRMED: the game tag / trigger name of the effect (the viewer drives them by name)
                e.insert("mode".into(), json!(m));
                e.insert("tag".into(), json!(h.tag));
            }
            for (k, v) in r {
                let empty = v.is_null() || v.as_array().map_or(false, Vec::is_empty) || v.as_str() == Some("");
                if !empty {
                    e.insert(k, v);
                }
            }
            out.push(e);
        }
        out
    }

    /// All sound entries of a ship: its own record, the loadout items, and (`containers`) the entities of its object containers.
    pub fn ship_sounds(&self, p4k: &P4k, cls: &str, loadout: &dyn LoadoutSource, containers: bool) -> Vec<Entry> {
        let mut entries = self.entity_hits(cls, None, Some(cls), &[], true);
        let mut seen: HashMap<String, usize> = HashMap::new();
        seen.insert(cls.to_string(), 1);
        let placed = loadout.loadout_items(cls);
        // every place the item sits in the ship (a weapon fires from its mount)
        let mut mounts: HashMap<&str, Vec<Value>> = HashMap::new();
        for p in &placed {
            if let (Some(c), Some(m)) = (p.cls.as_deref().filter(|c| !c.is_empty()), &p.matrix) {
                mounts.entry(c).or_default().push(json!([round3(m[3]), round3(m[7]), round3(m[11]), p.port.as_deref().unwrap_or("")]));
            }
        }
        for p in &placed {
            let Some(c) = p.cls.as_deref().filter(|c| !c.is_empty()) else { continue };
            if !seen.contains_key(c) {
                seen.insert(c.to_string(), 1);
                let nm = p.port.as_deref().filter(|s| !s.is_empty()).unwrap_or(c);
                let mut es = self.entity_hits(c, None, Some(nm), &[], true);
                if let Some(mt) = mounts.get(c) {
                    for e in &mut es {
                        // EMPIRICAL placement of the default loadout; the viewer plays from there
                        e.insert("mounts".into(), Value::Array(mt.iter().take(12).cloned().collect()));
                    }
                }
                entries.extend(es);
            } else {
                *seen.get_mut(c).unwrap() += 1;
            }
        }
        if containers {
            entries.extend(self.container_hits(p4k, cls, &mut seen));
        }
        entries
    }

    pub fn container_hits(&self, p4k: &P4k, cls: &str, seen: &mut HashMap<String, usize>) -> Vec<Entry> {
        let Some(conts) = ship_containers(self.items, cls) else { return vec![] };
        let cidx = p4k.lower_index(".socpak");
        let mut out = Vec::new();
        let mut done: HashSet<String> = HashSet::new();
        #[allow(clippy::too_many_arguments)]
        fn walk(me: &ShipSounds, p4k: &P4k, cidx: &p4k::archive::LowerIndex, name: &str, world: &[f64; 12], depth: usize, seen: &mut HashMap<String, usize>, done: &mut HashSet<String>, out: &mut Vec<Entry>) {
            if done.contains(name) || depth > 5 {
                return;
            }
            done.insert(name.to_string());
            let Some(entry) = p4k.get(name) else { return };
            let Ok(data) = p4k.read(entry) else { return };
            let Ok((ents, _errs)) = socpak::read_entities(&data, None, None) else { return };
            for en in ents {
                let m = compose(world, &en.matrix);
                if let Some(container) = en.container.as_deref().filter(|c| !c.is_empty()) {
                    let key = if container.starts_with("data/") { container.to_string() } else { format!("data/{}", container) };
                    if let Some(&i) = cidx.map.get(&key) {
                        let child = p4k.entries[i].name.to_string();
                        walk(me, p4k, cidx, &child, &m, depth + 1, seen, done, out);
                    }
                    continue;
                }
                let Some(c) = en.cls.as_deref().filter(|c| !c.is_empty()) else { continue };
                let first = !seen.contains_key(c);
                *seen.entry(c.to_string()).or_insert(0) += 1;
                if first || !en.audio.is_empty() {
                    out.extend(me.entity_hits(c, Some([round3(m[3]), round3(m[7]), round3(m[11])]), Some(&en.name), &en.audio, first));
                }
            }
        }
        for c in conts {
            if let Some(&i) = cidx.map.get(&data_key(&c.file)) {
                let name = p4k.entries[i].name.to_string();
                walk(self, p4k, &cidx, &name, &offset_matrix(c.pos, c.rot), 0, seen, &mut done, &mut out);
            }
        }
        out
    }
}

/// Counts: total, resolved to an event with media, event without media, unresolved (+ reasons).
pub fn stats(entries: &[Entry]) -> (Map<String, Value>, Map<String, Value>) {
    let mut c: Vec<(&str, u64)> = Vec::new();
    let bump = |c: &mut Vec<(&'static str, u64)>, k: &'static str| match c.iter_mut().find(|(n, _)| *n == k) {
        Some(s) => s.1 += 1,
        None => c.push((k, 1)),
    };
    let mut cc: Vec<(&'static str, u64)> = Vec::new();
    let mut why: Map<String, Value> = Map::new();
    for e in entries {
        bump(&mut cc, "total");
        let has = |k: &str| e.get(k).map_or(false, |v| v.as_array().map_or(!v.is_null() && v.as_str() != Some(""), |a| !a.is_empty()));
        if has("media") {
            bump(&mut cc, "media");
        } else if e.get("kind").and_then(Value::as_str) == Some("stop") {
            bump(&mut cc, "stop_only"); // a Stop_ event plays nothing by design
        } else if has("reason") {
            bump(&mut cc, "unresolved");
            let r = e["reason"].as_str().unwrap_or("").to_string();
            let n = why.get(&r).and_then(Value::as_u64).unwrap_or(0) + 1;
            why.insert(r, json!(n));
        } else {
            bump(&mut cc, "no_media");
        }
        if has("banks") {
            bump(&mut cc, "bank");
        }
    }
    c.clear();
    (cc.into_iter().map(|(k, v)| (k.to_string(), json!(v))).collect(), why)
}

/// Entries grouped by system in `ORDER` (empty groups dropped).
pub fn summarize(entries: &[Entry]) -> Vec<(String, Vec<&Entry>)> {
    let mut by: Vec<(String, Vec<&Entry>)> = ORDER.iter().map(|s| (s.to_string(), vec![])).collect();
    for e in entries {
        let s = e["system"].as_str().unwrap_or("Other");
        match by.iter_mut().find(|(n, _)| n == s) {
            Some(g) => g.1.push(e),
            None => by.push((s.to_string(), vec![e])),
        }
    }
    by.into_iter().filter(|(_, v)| !v.is_empty()).collect()
}
