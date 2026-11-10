//! Interaction points of a zone or ship: doors that open and close, seats and turret seats you can sit in, panels, ladders.
//! Evidence notes (EMPIRICAL on the live archive / CONFIRMED against the engine / INFERRED): door clips are key tracks in a
//! `.dba` (absolute LOCAL transforms, frame-0 key = rest local in 787 of 801 position tracks), the clip is scrubbed by progress = time / duration (the engine's door update); doors whose bones have no
//! decodable track slide along the long side of their box (APPROX_NOTE); the seat eye is the sit helper + SEAT_EYE (INFERRED), the exit
//! the model's exit helper or behind the seat.
//!
//! A decoded record is a `serde_json::Value` (`Value::to_json`), a description is the typed `Desc`, the caches sit behind mutexes, and every call that needs a node table takes the `GeoSource` (archive + overlay) explicitly.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use p4k_formats::adb;
use p4k_formats::dba::{self, Clip, Dba};
use p4k_formats::dcb::{Dcb, Value as Dv, NULL_PTR};
use p4k_formats::ivo::{self, Cgf, Node};
use p4k_formats::socpak::{compose, quat_scale_pos_matrix};
use p4k::P4k;
use regex::Regex;
use serde_json::{json, Value};

use crate::bed::{self, round_n, BedInfo};
use crate::builder::{companion_exts, GeoSource};
use crate::itemmaterials::ItemMaterials;
use crate::station::{self, jnum, jstr, truthy, OperatorModes, ResolvedJoint, TurretSpec};

pub type Mat = [f64; 12];
pub type V3 = [f64; 3];
pub const IDENT: Mat = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];

pub const SEAT_EYE: f64 = 0.62;
pub const EXIT_BACK: f64 = 0.9;
pub const APPROX_NOTE: &str = "some doors move approximately: their animation has no decodable key track, so each animated part slides along the long side of its bounding box by its own width";
pub const REACH_STEPS: usize = 24;
const SKIP_PREFIXES: [&str; 4] = ["NPC_", "Cloth", "Wear", "Mannequin"];
const RECORD: &str = "EntityClassDefinition.";

fn res() -> &'static Regexes {
    static R: OnceLock<Regexes> = OnceLock::new();
    R.get_or_init(|| Regexes {
        bed_class: Regex::new(r"(^|_)Bed(_|$)").unwrap(),
        exit: Regex::new("(?i)exit").unwrap(),
        console: Regex::new("(?i)console|terminal|kiosk").unwrap(),
        elevator: Regex::new(r"(?i)elevator|transitinteraction|transitdisplay|_elev\b").unwrap(),
        foot: Regex::new(r"(?i)^ik_foot|_foot_|^foot").unwrap(),
        words: Regex::new(r"[a-z0-9]+").unwrap(),
    })
}

struct Regexes {
    bed_class: Regex,
    exit: Regex,
    console: Regex,
    elevator: Regex,
    foot: Regex,
    words: Regex,
}

// ---------------------------------------------------------------------------------------------------------------- descriptions
/// What an entity class is, as `Catalog::describe` answers (a `kind` and kind-specific fields).
#[derive(Debug, Clone, Default)]
pub struct Desc {
    /// door | seat | turret | aim | ladder | panel
    pub kind: &'static str,
    pub radius: f64,
    // door
    pub open: bool,
    pub speed: f64,
    pub auto_close: f64,
    // ladder
    pub height: f64,
    pub approach_top: f64,
    pub back: bool,
    pub left: bool,
    pub right: bool,
    // seat / turret
    pub bone: String,
    /// 'seat' (SCItemSeatParams) | 'usable' (SActorUsableParams)
    pub source: &'static str,
    pub seat_type: String,
    pub yaw: [Option<f64>; 2],
    pub pitch: [Option<f64>; 2],
    pub bed: Option<BedInfo>,
    pub adb: String,
    pub mtag: String,
    pub trans: String,
    pub ctl: String,
    /// the seat access entrance's (enter, exit) states, set by the zone compile
    pub ent: Option<(adb::Skip, adb::Skip)>,
    pub modes: Option<OperatorModes>,
    pub subkind: &'static str,
    pub cmds: Vec<Value>,
    pub btns: Vec<Value>,
    pub turret: Option<TurretSpec>,
    // panel
    pub sub: &'static str,
    pub ops: Vec<&'static str>,
    pub names: Vec<String>,
    pub primary: Vec<String>,
}

impl Desc {
    pub fn is_seatlike(&self) -> bool {
        matches!(self.kind, "seat" | "turret")
    }
}

/// Collect every object of DataCore struct `name` below `v` (parents before children, property order).
pub fn find_struct<'a>(v: &'a Value, name: &str, out: &mut Vec<&'a Value>, depth: usize) {
    if depth > 24 {
        return;
    }
    match v {
        Value::Object(o) => {
            if o.get("__struct__").and_then(Value::as_str) == Some(name) {
                out.push(v);
            }
            for x in o.values() {
                find_struct(x, name, out, depth + 1);
            }
        }
        Value::Array(a) => {
            for x in a {
                find_struct(x, name, out, depth + 1);
            }
        }
        _ => {}
    }
}

fn num_or(v: Option<&Value>, default: f64) -> f64 {
    match v {
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f != 0.0 {
                f
            } else {
                default
            }
        }
        _ => default,
    }
}

fn opt_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

fn iname(i: &Value) -> String {
    match i.get("Interaction") {
        Some(it) if it.is_object() => jstr(it, "Name").to_string(),
        _ => String::new(),
    }
}

fn interaction_names(obj: &Value) -> Vec<String> {
    let mut ips = vec![];
    find_struct(obj, "SInteractionPointParams", &mut ips, 0);
    let mut out = vec![];
    for ip in ips {
        for i in ip.get("Interactions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            if i.is_object() {
                out.push(iname(i));
            }
        }
    }
    out
}

fn bed_info(slots: &[&Value], sit: Option<&Value>, lie: &Value) -> BedInfo {
    let mut main: Option<&'static str> = None;
    for sl in slots {
        let mut ips = vec![];
        find_struct(sl, "SInteractionPointParams", &mut ips, 0);
        for ip in ips {
            let names: Vec<String> = ip.get("Interactions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().filter(|i| i.is_object()).map(iname).collect();
            let first = names.iter().find(|n| *n == "Sit" || *n == "LieDown");
            if let Some(f) = first {
                if main.is_none() {
                    main = Some(if f == "Sit" { "sit" } else { "lie" });
                }
            }
        }
    }
    let modes: Vec<&'static str> = if sit.is_some() { vec!["sit", "lie"] } else { vec!["lie"] };
    let main = match main {
        Some(m) if modes.contains(&m) => m,
        _ => modes[0],
    };
    BedInfo { modes, main, lie: jstr(lie, "helperName").to_string(), sit: sit.map(|s| jstr(s, "helperName").to_string()), inferred: false }
}

fn panel_desc(inter: &Value, cls: &str, radius: Option<f64>) -> Option<Desc> {
    let mut ips = vec![];
    let mut shared = vec![];
    find_struct(inter, "SInteractionPointParams", &mut ips, 0);
    find_struct(inter, "SSharedInteractionParams", &mut shared, 0);
    let mut names: Vec<String> = vec![];
    for sh in shared {
        let n = jstr(sh, "Name");
        if !n.is_empty() && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    }
    let have = |n: &str| names.iter().any(|x| x == n);
    let r = res();
    let (sub, ops): (&'static str, Vec<&'static str>);
    if have("CallElevator") || have("Request") || have("CancelStop") || r.elevator.is_match(cls) {
        sub = "elevator";
        ops = vec!["call"];
    } else if have("OpenDoor") && have("CloseDoor") {
        sub = "door";
        ops = if have("LockDoor") && have("UnlockDoor") { vec!["open", "close", "lock", "unlock"] } else { vec!["open", "close"] };
    } else if have("LightsOn") && have("LightsOff") {
        sub = "lights";
        ops = vec!["on", "low", "alarm", "off"];
    } else if r.console.is_match(cls) {
        sub = "console";
        ops = vec![];
    } else {
        return None;
    }
    if cls.starts_with("AttachSlot") {
        return None;
    }
    let rad = ips.first().and_then(|ip| ip.get("Radius"));
    let mut primary: Vec<String> = vec![];
    for ip in &ips {
        for it in ip.get("Interactions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
            let n = it.get("Interaction").map(|x| jstr(x, "Name")).unwrap_or("");
            if truthy(it.get("IsPrimary")) && !n.is_empty() && !primary.iter().any(|p| p == n) {
                primary.push(n.to_string());
            }
        }
    }
    let radius_v = num_or(rad, num_or(radius.map(|r| json!(r)).as_ref(), 1.6));
    Some(Desc { kind: "panel", sub, ops, names, primary, radius: radius_v, ..Default::default() })
}

// ---------------------------------------------------------------------------------------------------------------- math
pub fn inv34(m: &Mat) -> Mat {
    let (a, b, c, d, e, f, g, h, k) = (m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10]);
    let mut det = a * (e * k - f * h) - b * (d * k - f * g) + c * (d * h - e * g);
    if det == 0.0 {
        det = 1e-12;
    }
    let r = [
        (e * k - f * h) / det,
        (c * h - b * k) / det,
        (b * f - c * e) / det,
        (f * g - d * k) / det,
        (a * k - c * g) / det,
        (c * d - a * f) / det,
        (d * h - e * g) / det,
        (b * g - a * h) / det,
        (a * e - b * d) / det,
    ];
    let t = [m[3], m[7], m[11]];
    [
        r[0], r[1], r[2], -(r[0] * t[0] + r[1] * t[1] + r[2] * t[2]),
        r[3], r[4], r[5], -(r[3] * t[0] + r[4] * t[1] + r[5] * t[2]),
        r[6], r[7], r[8], -(r[6] * t[0] + r[7] * t[1] + r[8] * t[2]),
    ]
}

/// xyzw quaternion of the rotation part of a 3x4 matrix.
pub fn mat_to_quat(m: &Mat) -> [f64; 4] {
    let (r00, r01, r02, r10, r11, r12, r20, r21, r22) = (m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10]);
    let tr = r00 + r11 + r22;
    if tr > 0.0 {
        let s = (tr + 1.0).sqrt() * 2.0;
        return [(r21 - r12) / s, (r02 - r20) / s, (r10 - r01) / s, s / 4.0];
    }
    if r00 >= r11 && r00 >= r22 {
        let s = (1.0 + r00 - r11 - r22).sqrt() * 2.0;
        return [s / 4.0, (r01 + r10) / s, (r02 + r20) / s, (r21 - r12) / s];
    }
    if r11 >= r22 {
        let s = (1.0 + r11 - r00 - r22).sqrt() * 2.0;
        return [(r01 + r10) / s, s / 4.0, (r12 + r21) / s, (r02 - r20) / s];
    }
    let s = (1.0 + r22 - r00 - r11).sqrt() * 2.0;
    [(r02 + r20) / s, (r12 + r21) / s, s / 4.0, (r10 - r01) / s]
}

pub fn local_matrix(q: [f64; 4], p: V3) -> Mat {
    quat_scale_pos_matrix([q[3], q[0], q[1], q[2]], [1.0; 3], p)
}

/// Python's `math.dist` for three coordinates: the sum of squares is carried in double-double and the square root is corrected, so the
/// result is the correctly rounded norm like CPython's (a plain `sqrt(sum)` differs in the last bit and flips 4-decimal roundings).
fn dist(a: V3, b: V3) -> f64 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let (mut hi, mut lo) = (0.0f64, 0.0f64);
    for x in d {
        let p = x * x;
        let pe = x.mul_add(x, -p);
        let s = hi + p;
        let bb = s - hi;
        let se = (hi - (s - bb)) + (p - bb);
        hi = s;
        lo += se + pe;
    }
    let (h2, l2) = (hi + lo, lo - ((hi + lo) - hi));
    let r = h2.sqrt();
    if r == 0.0 || !r.is_finite() {
        return r;
    }
    let err = (-r).mul_add(r, h2) + l2;
    r + err / (2.0 * r)
}

fn xf(m: &Mat, p: V3) -> V3 {
    ivo::xform(m, p)
}

fn r3(v: V3, nd: i32) -> Vec<f64> {
    v.iter().map(|x| round_n(*x, nd)).collect()
}

/// Axis-aligned box of the corners of `b` moved by `m`.
pub fn box_xf(m: &Mat, b: &[V3; 2]) -> [V3; 2] {
    let (mut lo, mut hi) = ([1e30; 3], [-1e30; 3]);
    for x in [b[0][0], b[1][0]] {
        for y in [b[0][1], b[1][1]] {
            for z in [b[0][2], b[1][2]] {
                let c = xf(m, [x, y, z]);
                for k in 0..3 {
                    lo[k] = f64::min(lo[k], c[k]);
                    hi[k] = f64::max(hi[k], c[k]);
                }
            }
        }
    }
    [lo, hi]
}

/// Axis-aligned box (min, max) in object space of the meshes of nodes[idxs]; None when none has a mesh.
fn box_of(nodes: &[Node], idxs: &[usize]) -> Option<[V3; 2]> {
    let (mut lo, mut hi) = ([1e30; 3], [-1e30; 3]);
    for &i in idxs {
        let n = &nodes[i];
        if n.num_tris == 0 {
            continue;
        }
        let b = n.bbox;
        for x in [b[0], b[3]] {
            for y in [b[1], b[4]] {
                for z in [b[2], b[5]] {
                    let p = xf(&n.world, [x, y, z]);
                    for k in 0..3 {
                        lo[k] = f64::min(lo[k], p[k]);
                        hi[k] = f64::max(hi[k], p[k]);
                    }
                }
            }
        }
    }
    if lo[0] <= hi[0] {
        Some([lo, hi])
    } else {
        None
    }
}

fn far(nodes: &[Node], idxs: &[usize], origin: V3) -> f64 {
    let mut best = 0.0f64;
    for &i in idxs {
        let n = &nodes[i];
        if n.num_tris > 0 {
            let b = n.bbox;
            for x in [b[0], b[3]] {
                for y in [b[1], b[4]] {
                    for z in [b[2], b[5]] {
                        best = best.max(dist(origin, xf(&n.world, [x, y, z])));
                    }
                }
            }
        }
    }
    best
}

// ---------------------------------------------------------------------------------------------------------------- joints
/// Keys of one joint of a clip: times in seconds, flat xyzw quaternions and flat xyz positions (rounded as the feed).
#[derive(Debug, Clone)]
pub struct Keys {
    pub t: Vec<f64>,
    pub q: Option<Vec<f64>>,
    pub p: Option<Vec<f64>>,
}

impl Keys {
    pub fn to_json(&self) -> Value {
        json!({"t": self.t, "q": self.q, "p": self.p})
    }
}

fn joint_keys(track: Option<&dba::Track>) -> Option<Keys> {
    let track = track?;
    let mut ts: Vec<f64> = vec![];
    if let Some((t, _)) = &track.rot {
        ts.extend(t);
    }
    if let Some((t, _)) = &track.pos {
        ts.extend(t);
    }
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ts.dedup();
    if ts.is_empty() {
        return None;
    }
    let q = track.rot.as_ref().map(|(t, v)| ts.iter().flat_map(|x| dba::sample_rot(t, v, *x)).map(|v| round_n(v, 5)).collect());
    let p = track.pos.as_ref().map(|(t, v)| ts.iter().flat_map(|x| dba::sample_pos(t, v, *x)).map(|v| round_n(v, 5)).collect());
    Some(Keys { t: ts.iter().map(|t| round_n(t / dba::FPS, 4)).collect(), q, p })
}

/// (xyzw or None, xyz or None) of joint keys at `t` seconds: the same blend as the viewer's JS.
pub fn sample_keys(keys: Option<&Keys>, t: f64) -> (Option<[f64; 4]>, Option<V3>) {
    let Some(k) = keys else { return (None, None) };
    let f = t * dba::FPS;
    let times: Vec<f64> = k.t.iter().map(|x| x * dba::FPS).collect();
    let q = k.q.as_ref().map(|q| {
        let vals: Vec<[f64; 4]> = (0..times.len()).map(|i| [q[4 * i], q[4 * i + 1], q[4 * i + 2], q[4 * i + 3]]).collect();
        dba::sample_rot(&times, &vals, f)
    });
    let p = k.p.as_ref().map(|p| {
        let vals: Vec<[f64; 3]> = (0..times.len()).map(|i| [p[3 * i], p[3 * i + 1], p[3 * i + 2]]).collect();
        dba::sample_pos(&times, &vals, f)
    });
    (q, p)
}

fn moves(keys: &Option<Keys>) -> bool {
    let Some(k) = keys else { return false };
    for (v, n) in [(&k.q, 4usize), (&k.p, 3usize)] {
        if let Some(v) = v {
            if (0..v.len()).any(|i| (v[i] - v[i % n]).abs() > 1e-4) {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone)]
pub struct DJoint {
    pub n: String,
    pub par: i64,
    pub p: Mat,
    pub lq: [f64; 4],
    pub lp: V3,
    pub wi: Mat,
    pub o: Option<Keys>,
    pub c: Option<Keys>,
    pub ax: Option<&'static str>,
}

impl DJoint {
    pub fn to_json(&self) -> Value {
        let mut v = json!({"n": self.n, "par": self.par, "P": self.p.iter().map(|x| round_n(*x, 6)).collect::<Vec<_>>(),
                           "lq": self.lq.iter().map(|x| round_n(*x, 6)).collect::<Vec<_>>(),
                           "lp": self.lp.iter().map(|x| round_n(*x, 6)).collect::<Vec<_>>(),
                           "o": self.o.as_ref().map(Keys::to_json), "c": self.c.as_ref().map(Keys::to_json)});
        if let Some(ax) = self.ax {
            v["ax"] = json!(ax);
        }
        v
    }
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    pub bone: String,
    pub nodes: Vec<usize>,
    pub j: Option<usize>,
    pub wi: Option<Vec<f64>>,
    pub reach: Option<f64>,
    pub d: Option<Vec<f64>>,
}

impl Group {
    /// The feed form (without `nodes`).
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("bone".into(), json!(self.bone));
        if let Some(j) = self.j {
            m.insert("j".into(), json!(j));
        }
        if let Some(w) = &self.wi {
            m.insert("Wi".into(), json!(w));
        }
        if let Some(r) = self.reach {
            m.insert("reach".into(), json!(r));
        }
        if let Some(d) = &self.d {
            m.insert("d".into(), json!(d));
        }
        Value::Object(m)
    }
}

/// How a door / seat / turret model splits into moving parts (`split_door`).
#[derive(Debug, Clone)]
pub struct Dg {
    pub seconds: f64,
    pub seconds_close: f64,
    pub groups: Vec<Group>,
    pub joints: Vec<DJoint>,
    pub rest: Option<Vec<usize>>,
    pub moving: Vec<usize>,
    pub ip: Option<V3>,
    pub centre: V3,
    pub approx: bool,
    pub real: bool,
    pub box_: Option<[V3; 2]>,
    pub sweep: Option<[V3; 2]>,
    pub jof: HashMap<String, usize>,
    pub turret: Option<Vec<ResolvedJoint>>,
    pub ext: Option<f64>,
}

impl Dg {
    /// The `models[]` entry of the feed.
    pub fn model_json(&self) -> Value {
        json!({"so": round_n(self.seconds, 4), "sc": round_n(self.seconds_close, 4), "joints": self.joints.iter().map(DJoint::to_json).collect::<Vec<_>>(),
               "groups": self.groups.iter().map(Group::to_json).collect::<Vec<_>>()})
    }
}

/// [3x4 skinning matrix per joint] at `t` seconds of clip 'o' (open) or 'c' (close): world(t) * inverse(rest world).
fn joint_skin(joints: &[DJoint], clip_o: bool, t: f64) -> Vec<Mat> {
    let mut worlds: Vec<Mat> = vec![];
    let mut out = vec![];
    for jt in joints {
        let (q, p) = sample_keys(if clip_o { jt.o.as_ref() } else { jt.c.as_ref() }, t);
        let local = local_matrix(q.unwrap_or(jt.lq), p.unwrap_or(jt.lp));
        let base = if jt.par >= 0 { compose(&worlds[jt.par as usize], &jt.p) } else { jt.p };
        let w = compose(&base, &local);
        worlds.push(w);
        out.push(compose(&w, &jt.wi));
    }
    out
}

fn reach(nodes: &[Node], idxs: &[usize], joints: &[DJoint], ji: usize, dur: f64) -> f64 {
    let mut corners: Vec<V3> = vec![];
    for &i in idxs {
        let n = &nodes[i];
        if n.num_tris > 0 {
            let b = n.bbox;
            for x in [b[0], b[3]] {
                for y in [b[1], b[4]] {
                    for z in [b[2], b[5]] {
                        corners.push(xf(&n.world, [x, y, z]));
                    }
                }
            }
        }
    }
    let mut best = 0.0f64;
    for clip_o in [true, false] {
        if !joints.iter().any(|j| if clip_o { j.o.is_some() } else { j.c.is_some() }) {
            continue;
        }
        for k in 0..=REACH_STEPS {
            let m = joint_skin(joints, clip_o, dur * k as f64 / REACH_STEPS as f64)[ji];
            for c in &corners {
                best = best.max(dist(*c, xf(&m, *c)));
            }
        }
    }
    best
}

fn swept_box(nodes: &[Node], own: &BTreeMap<usize, Vec<usize>>, bones: &[usize], joints: &[DJoint], slide: &HashMap<usize, V3>, dur: f64) -> Option<[V3; 2]> {
    let (mut lo, mut hi) = ([1e30; 3], [-1e30; 3]);
    let jidx: HashMap<&str, usize> = joints.iter().enumerate().map(|(k, j)| (j.n.as_str(), k)).collect();
    let mut add = |c: V3| {
        for k in 0..3 {
            lo[k] = f64::min(lo[k], c[k]);
            hi[k] = f64::max(hi[k], c[k]);
        }
    };
    for &b in bones {
        let Some(bx) = box_of(nodes, &own[&b]) else { continue };
        let mut corners = vec![];
        for x in [bx[0][0], bx[1][0]] {
            for y in [bx[0][1], bx[1][1]] {
                for z in [bx[0][2], bx[1][2]] {
                    corners.push([x, y, z]);
                }
            }
        }
        for c in &corners {
            add(*c);
        }
        if let Some(&ji) = jidx.get(nodes[b].name.as_str()) {
            for clip_o in [true, false] {
                if !joints.iter().any(|j| if clip_o { j.o.is_some() } else { j.c.is_some() }) {
                    continue;
                }
                for k in 0..=REACH_STEPS {
                    let m = joint_skin(joints, clip_o, dur * k as f64 / REACH_STEPS as f64)[ji];
                    for c in &corners {
                        add(xf(&m, *c));
                    }
                }
            }
        } else if let Some(s) = slide.get(&b) {
            for c in &corners {
                add([c[0] + s[0], c[1] + s[1], c[2] + s[2]]);
            }
        }
    }
    if lo[0] <= hi[0] {
        Some([lo, hi])
    } else {
        None
    }
}

fn turn_reach(nodes: &[Node], idxs: &[usize], parent: &HashMap<usize, usize>, tjoint: &HashMap<usize, &ResolvedJoint>) -> f64 {
    let mut best = 0.0f64;
    if let Some(&i) = idxs.first() {
        let mut a = Some(i);
        while let Some(x) = a {
            if tjoint.contains_key(&x) {
                best = best.max(2.0 * far(nodes, idxs, [nodes[x].world[3], nodes[x].world[7], nodes[x].world[11]]));
            }
            a = parent.get(&x).copied();
        }
    }
    best
}

/// Iteration order of a CPython `set` of small ints after adding `keys` one by one (hash(i) == i, open addressing with 9 linear probes,
/// perturbed re-probing, 4x growth at 3/5 fill; Objects/setobject.c, EMPIRICAL: the joint order of the door models, which follows the
/// original Python implementation, depends on it).
pub fn py_set_order(keys: &[usize]) -> Vec<usize> {
    fn insert_clean(table: &mut [Option<usize>], key: usize) {
        let mask = table.len() - 1;
        let (mut i, mut perturb) = (key & mask, key);
        loop {
            if table[i].is_none() {
                table[i] = Some(key);
                return;
            }
            if i + 9 <= mask {
                for j in 1..=9 {
                    if table[i + j].is_none() {
                        table[i + j] = Some(key);
                        return;
                    }
                }
            }
            perturb >>= 5;
            i = (i * 5 + 1 + perturb) & mask;
        }
    }
    let mut table: Vec<Option<usize>> = vec![None; 8];
    let (mut fill, mut seen) = (0usize, HashSet::new());
    for &key in keys {
        if !seen.insert(key) {
            continue;
        }
        insert_clean(&mut table, key);
        fill += 1;
        let mask = table.len() - 1;
        if fill * 5 >= mask * 3 {
            let minused = if fill > 50000 { fill * 2 } else { fill * 4 };
            let mut size = 8;
            while size <= minused {
                size <<= 1;
            }
            let mut nt: Vec<Option<usize>> = vec![None; size];
            for k in table.iter().flatten() {
                insert_clean(&mut nt, *k);
            }
            table = nt;
        }
    }
    table.into_iter().flatten().collect()
}

/// Group a door model's nodes by the animated bones (CRC32 of the node name in the clips' bone lists).  `approx=false` (seats): only
/// bones whose keys really change are animated; `turret` adds the rotating joints of a turret as bones without keys.
pub fn split_door(nodes: &[Node], open_clip: Option<&Clip>, close_clip: Option<&Clip>, approx: bool, turret: Option<&[ResolvedJoint]>) -> Option<Dg> {
    let seconds = open_clip.map_or(0.0, |c| c.seconds);
    let seconds_close = close_clip.map_or(0.0, |c| c.seconds);
    let mut ids: HashSet<u32> = HashSet::new();
    if let Some(c) = open_clip {
        ids.extend(c.bones.iter().copied());
    }
    if let Some(c) = close_clip {
        ids.extend(c.bones.iter().copied());
    }
    let empty = dba::Tracks::default();
    let t_open = open_clip.map_or(&empty, |c| c.tracks());
    let t_close = close_clip.map_or(&empty, |c| c.tracks());
    let tjoint: HashMap<usize, &ResolvedJoint> = turret.unwrap_or(&[]).iter().map(|j| (j.ni, j)).collect();
    let mut parent: HashMap<usize, usize> = HashMap::new();
    for n in nodes {
        for &c in &n.children {
            parent.insert(c as usize, n.index);
        }
    }
    let bid: Vec<Option<u32>> = nodes.iter().map(|n| dba::bone_id(&n.name, &ids)).collect();
    let mut bones: Vec<usize> = nodes.iter().filter(|n| bid[n.index].is_some()).map(|n| n.index).collect();
    if !approx {
        bones.retain(|&i| {
            let c = bid[i].unwrap();
            moves(&joint_keys(t_open.get(c))) || moves(&joint_keys(t_close.get(c)))
        });
    }
    if !tjoint.is_empty() {
        let keep: HashSet<usize> = bones.iter().copied().chain(tjoint.keys().copied()).collect();
        bones = nodes.iter().filter(|n| keep.contains(&n.index)).map(|n| n.index).collect();
    }
    if bones.is_empty() {
        return None;
    }
    let bone_set: HashSet<usize> = bones.iter().copied().collect();
    let subtree = |i: usize, stop: bool| -> Vec<usize> {
        let (mut out, mut todo) = (vec![], vec![i]);
        while let Some(k) = todo.pop() {
            out.push(k);
            todo.extend(nodes[k].children.iter().map(|c| *c as usize).filter(|c| !(stop && bone_set.contains(c))));
        }
        out
    };
    let own: BTreeMap<usize, Vec<usize>> = bones.iter().map(|&b| (b, subtree(b, true))).collect();
    let full: HashMap<usize, Vec<usize>> = bones.iter().map(|&b| (b, subtree(b, false))).collect();
    let moving: HashSet<usize> = own.values().flatten().copied().collect();
    let rest: Vec<usize> = nodes.iter().map(|n| n.index).filter(|i| !moving.contains(i)).collect();
    let boxes: HashMap<usize, Option<[V3; 2]>> = own.keys().map(|&b| (b, box_of(nodes, &full[&b]))).collect();
    let live: Vec<usize> = bones.iter().copied().filter(|b| boxes[b].is_some()).collect();
    if live.is_empty() && tjoint.is_empty() {
        return None;
    }
    let meshy: Vec<usize> = live.iter().copied().filter(|b| own[b].iter().any(|&i| nodes[i].num_tris > 0)).collect();
    if meshy.is_empty() && tjoint.is_empty() {
        return None;
    }
    let depth = |mut i: usize| -> usize {
        let mut d = 0;
        while let Some(&p) = parent.get(&i) {
            i = p;
            d += 1;
        }
        d
    };
    let has_keys: HashSet<usize> = bones
        .iter()
        .copied()
        .filter(|&b| bid[b].map_or(false, |c| joint_keys(t_open.get(c)).is_some() || joint_keys(t_close.get(c)).is_some()))
        .chain(tjoint.keys().copied())
        .collect();
    let mut need: Vec<usize> = vec![]; // insertion order of the emulated set (its iteration order depends on it)
    let tj_order: Vec<usize> = turret.unwrap_or(&[]).iter().map(|j| j.ni).collect(); // dict order of `tjoint`
    for b in meshy.iter().copied().chain(tj_order) {
        let mut i = Some(b);
        while let Some(x) = i {
            if has_keys.contains(&x) && !need.contains(&x) {
                need.push(x);
            }
            i = parent.get(&x).copied();
        }
    }
    let mut jn: Vec<usize> = py_set_order(&need);
    jn.sort_by_key(|&i| depth(i)); // stable: ties keep the set's iteration order
    let jidx: HashMap<usize, usize> = jn.iter().enumerate().map(|(k, &b)| (b, k)).collect();
    let mut joints: Vec<DJoint> = vec![];
    for &b in &jn {
        let pn = parent.get(&b).copied();
        let wp = pn.map_or(IDENT, |p| nodes[p].world);
        let mut a = pn;
        while let Some(x) = a {
            if jidx.contains_key(&x) {
                break;
            }
            a = parent.get(&x).copied();
        }
        let crc = bid[b];
        let nb = &nodes[b];
        let p = match a {
            Some(x) => compose(&inv34(&nodes[x].world), &wp),
            None => wp,
        };
        let lp = [nb.local[3], nb.local[7], nb.local[11]];
        let mut j = DJoint {
            n: nb.name.clone(),
            par: a.map_or(-1, |x| jidx[&x] as i64),
            p,
            lq: mat_to_quat(&nb.local),
            lp,
            wi: inv34(&nb.world),
            o: crc.and_then(|c| joint_keys(t_open.get(c))),
            c: crc.and_then(|c| joint_keys(t_close.get(c))),
            ax: None,
        };
        if let Some(t) = tjoint.get(&b) {
            j.ax = Some(t.j.ax);
        }
        joints.push(j);
    }
    let real: Vec<usize> = meshy.iter().copied().filter(|b| jidx.contains_key(b)).collect();
    let approx_b: Vec<usize> = meshy.iter().copied().filter(|b| !jidx.contains_key(b)).collect();
    let (mut lo, mut hi) = ([0.0; 3], [0.0; 3]);
    if !live.is_empty() {
        for k in 0..3 {
            lo[k] = live.iter().map(|b| boxes[b].unwrap()[0][k]).fold(f64::INFINITY, f64::min);
            hi[k] = live.iter().map(|b| boxes[b].unwrap()[1][k]).fold(f64::NEG_INFINITY, f64::max);
        }
    }
    let centre = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
    let mut slide: HashMap<usize, V3> = HashMap::new();
    for &b in &approx_b {
        let bx = boxes[&b].unwrap();
        let ext = [bx[1][0] - bx[0][0], bx[1][1] - bx[0][1], bx[1][2] - bx[0][2]];
        let axis = if ext[0] >= ext[1] { 0 } else { 1 };
        let mid = (bx[0][axis] + bx[1][axis]) / 2.0;
        let sign = if mid - centre[axis] >= -0.05 { 1.0 } else { -1.0 };
        let mut d = [0.0; 3];
        d[axis] = sign * (3.0f64).min((0.2f64).max(ext[axis] * 0.95));
        slide.insert(b, d);
    }
    let mut groups = vec![];
    for &b in &meshy {
        let mut g = Group { bone: nodes[b].name.clone(), nodes: own[&b].clone(), ..Default::default() };
        if let Some(&ji) = jidx.get(&b) {
            g.j = Some(ji);
            g.wi = Some(joints[ji].wi.iter().map(|v| round_n(*v, 6)).collect());
            let mut r = round_n(reach(nodes, &own[&b], &joints, ji, seconds.max(seconds_close)), 4);
            if !tjoint.is_empty() {
                r = round_n(r.max(turn_reach(nodes, &own[&b], &parent, &tjoint)), 4);
            }
            g.reach = Some(r);
        } else {
            let mut d = slide[&b];
            let mut p = parent.get(&b).copied();
            while let Some(x) = p {
                if let Some(s) = slide.get(&x) {
                    for k in 0..3 {
                        d[k] += s[k];
                    }
                }
                p = parent.get(&x).copied();
            }
            g.d = Some(d.iter().map(|v| round_n(*v, 4)).collect());
        }
        groups.push(g);
    }
    let has_mesh_rest = rest.iter().any(|&i| nodes[i].num_tris > 0);
    let ip = nodes
        .iter()
        .find(|n| n.name.to_lowercase().trim_start_matches('$') == "ip_openclose" && n.num_tris == 0)
        .or_else(|| nodes.iter().find(|n| n.name.to_lowercase().starts_with("$ip_") && n.num_tris == 0));
    let mut moving_v: Vec<usize> = moving.iter().copied().collect();
    moving_v.sort_unstable();
    let mut jof: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        let mut a = Some(n.index);
        while let Some(x) = a {
            if jidx.contains_key(&x) {
                break;
            }
            a = parent.get(&x).copied();
        }
        if let Some(x) = a {
            jof.insert(n.name.to_lowercase(), jidx[&x]);
        }
    }
    let ext = if !tjoint.is_empty() {
        let mesh_idx: Vec<usize> = nodes.iter().filter(|n| n.num_tris > 0).map(|n| n.index).collect();
        Some(round_n(far(nodes, &mesh_idx, [0.0; 3]).max(0.0), 3))
    } else {
        None
    };
    let sweep = if !live.is_empty() { swept_box(nodes, &own, &bones, &joints, &slide, seconds.max(seconds_close)) } else { None };
    // P / lq / lp are handed on rounded to 6 decimals; every later pose (node_world of seats, lift paths) uses these values, and the
    // 1e-7 noise they drop decides the sign of a seat's yaw near pi (EMPIRICAL: Idris bridge chairs)
    let joints: Vec<DJoint> = joints
        .into_iter()
        .map(|mut j| {
            j.p = j.p.map(|v| round_n(v, 6));
            j.lq = j.lq.map(|v| round_n(v, 6));
            j.lp = j.lp.map(|v| round_n(v, 6));
            j
        })
        .collect();
    Some(Dg {
        seconds,
        seconds_close,
        groups,
        joints,
        rest: if has_mesh_rest { Some(rest) } else { None },
        moving: moving_v,
        ip: ip.map(|n| [n.world[3], n.world[7], n.world[11]]),
        centre,
        approx: !approx_b.is_empty(),
        real: !real.is_empty(),
        box_: if live.is_empty() { None } else { Some([lo, hi]) },
        sweep,
        jof,
        turret: if tjoint.is_empty() { None } else { turret.map(|t| t.to_vec()) },
        ext,
    })
}

/// Model-space 3x4 world matrix of node `idx` with the joints posed by clip 'o' / 'c' at `t` seconds.
pub fn node_world(nodes: &[Node], idx: usize, joints: &[DJoint], clip_o: bool, t: f64) -> Mat {
    let mut parent: HashMap<usize, usize> = HashMap::new();
    for n in nodes {
        for &c in &n.children {
            parent.insert(c as usize, n.index);
        }
    }
    let mut chain = vec![];
    let mut i = Some(idx);
    while let Some(x) = i {
        chain.push(x);
        i = parent.get(&x).copied();
    }
    let by: HashMap<&str, &DJoint> = joints.iter().map(|j| (j.n.as_str(), j)).collect();
    let mut w = IDENT;
    for &i in chain.iter().rev() {
        let n = &nodes[i];
        let keys = by.get(n.name.as_str()).and_then(|j| if clip_o { j.o.as_ref() } else { j.c.as_ref() }.map(|k| (j, k)));
        let local = match keys {
            Some((j, k)) => {
                let (q, p) = sample_keys(Some(k), t);
                local_matrix(q.unwrap_or(j.lq), p.unwrap_or(j.lp))
            }
            None => n.local,
        };
        w = compose(&w, &local);
    }
    w
}

// ---------------------------------------------------------------------------------------------------------------- ladders
const LADDER_WIDTH: f64 = 0.9;

/// Feed entry for a placed `Ladder` entity (world matrix `m`), or None when it is degenerate.  `params` = the placed entity's overrides
/// (`height`, `approachAngleAtTop`).  EMPIRICAL: 888 entities in 422 containers.
pub fn ladder_item(desc: &Desc, params: &Value, m: &Mat, bx: Option<[V3; 2]>) -> Option<serde_json::Map<String, Value>> {
    let over_h = num_or(params.get("height"), 0.0);
    let h = if over_h != 0.0 { over_h } else if desc.height != 0.0 { desc.height } else { 8.0 };
    let col = |k: usize| [m[k], m[k + 4], m[k + 8]];
    let ax = [col(0), col(1), col(2)];
    let ln: Vec<f64> = ax.iter().map(|a| (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()).collect();
    if ln.iter().cloned().fold(f64::INFINITY, f64::min) < 1e-6 || h <= 0.0 {
        return None;
    }
    let unit = |i: usize| [ax[i][0] / ln[i], ax[i][1] / ln[i], ax[i][2] / ln[i]];
    let (x, y, u) = (unit(0), unit(1), unit(2));
    let b = [m[3], m[7], m[11]];
    let t = [b[0] + u[0] * h * ln[2], b[1] + u[1] * h * ln[2], b[2] + u[2] * h * ln[2]];
    let (lo, hi) = match bx {
        Some(b) => (b[0], b[1]),
        None => ([-LADDER_WIDTH / 2.0, -0.3, 0.0], [LADDER_WIDTH / 2.0, 0.3, h]),
    };
    let approach = match params.get("approach_top") {
        Some(v) if !v.is_null() => v.as_f64().unwrap_or(0.0),
        _ => desc.approach_top,
    };
    let mut o = serde_json::Map::new();
    let f3 = |v: V3| json!([round_n(v[0], 3), round_n(v[1], 3), round_n(v[2], 3)]);
    o.insert("b".into(), f3(b));
    o.insert("t".into(), f3(t));
    o.insert("u".into(), f3(u));
    o.insert("y".into(), f3(y));
    o.insert("x".into(), f3(x));
    o.insert("h".into(), json!(round_n(h * ln[2], 3)));
    o.insert("w".into(), json!(round_n((hi[0] - lo[0]) * ln[0], 3)));
    o.insert("ylo".into(), json!(round_n(lo[1] * ln[1], 3)));
    o.insert("yhi".into(), json!(round_n(hi[1] * ln[1], 3)));
    o.insert("back".into(), json!(desc.back));
    o.insert("left".into(), json!(desc.left));
    o.insert("right".into(), json!(desc.right));
    o.insert("approach_top".into(), json!(round_n(approach, 3)));
    Some(o)
}

// ---------------------------------------------------------------------------------------------------------------- catalog
#[derive(Default)]
struct Caches {
    desc: HashMap<String, Option<Arc<Desc>>>,
    cgf: HashMap<String, Option<Arc<Cgf>>>,
    groups: HashMap<String, Option<Arc<Dg>>>,
    chr: HashMap<String, (Option<Arc<Clip>>, Option<Arc<Clip>>)>,
    dba: HashMap<String, Option<Arc<Dba>>>,
    adbs: HashMap<String, Arc<Vec<adb::AdbOption>>>,
    clip: HashMap<(String, String, String), Option<Arc<Clip>>>,
    seatg: HashMap<String, Option<Arc<Dg>>>,
    access: HashMap<String, Vec<(String, adb::Skip, adb::Skip)>>,
}

/// Per-archive helper: classifies entity classes and reads door / seat data.
pub struct Catalog<'a> {
    pub items: &'a ItemMaterials,
    pub p4k: &'a P4k,
    c: Mutex<Caches>,
    clip_idx: OnceLock<HashMap<String, Vec<(String, String)>>>,
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

impl<'a> Catalog<'a> {
    pub fn new(items: &'a ItemMaterials, p4k: &'a P4k) -> Catalog<'a> {
        Catalog { items, p4k, c: Mutex::new(Caches::default()), clip_idx: OnceLock::new() }
    }

    pub fn ready(&self) -> bool {
        self.items.datacore().is_some()
    }

    fn dcb(&self) -> Option<&Dcb> {
        self.items.datacore()
    }

    /// Components of a class record decoded only as deep as `describe` needs, as JSON.
    fn record(&self, d: &Dcb, rec_idx: usize) -> Option<Vec<Value>> {
        let rec = d.record(rec_idx).ok()?;
        let top = d.record_data(&rec, 0).ok()?;
        let Some(Dv::List(comps)) = top.get("Components") else { return Some(vec![]) };
        let mut out = vec![];
        for c in comps {
            if let Dv::Strong(s, _) = c {
                if *s != NULL_PTR {
                    let name = d.struct_name(*s as usize).ok()?;
                    let depth = match name {
                        "SEntityInteractableParams" | "SCItemDoorParams" | "SCItemSeatParams" | "SCItemTurretParams" => 12,
                        "SActorUsableParams" => 6,
                        "SCItemSeatDashboardParams" => 5,
                        "LadderComponentParams" | "SAnimationControllerParams" => 3,
                        "SAttachableComponentParams" => 4,
                        _ => 0,
                    };
                    if depth > 0 {
                        out.push(d.deref(c.clone(), 0, depth).ok()?.to_json());
                    }
                }
            }
        }
        Some(out)
    }

    /// None, or the description of an entity class (cached).
    pub fn describe(&self, cls: &str) -> Option<Arc<Desc>> {
        if let Some(v) = self.c.lock().unwrap().desc.get(cls) {
            return v.clone();
        }
        let mut out: Option<Desc> = None;
        if !cls.is_empty() && self.ready() && !SKIP_PREFIXES.iter().any(|p| cls.starts_with(p)) {
            let d = self.dcb().unwrap();
            if let Some(ids) = d.record_index().get(&format!("{}{}", RECORD, cls)) {
                if let Some(comps) = self.record(d, ids[0]) {
                    out = Self::describe_comps(&comps, cls);
                    if let Some(o) = out.as_mut() {
                        if matches!(o.kind, "seat" | "turret" | "aim") {
                            self.station(o, &comps, cls);
                        }
                    }
                }
            }
        }
        let out = out.map(Arc::new);
        self.c.lock().unwrap().desc.insert(cls.to_string(), out.clone());
        out
    }

    fn describe_comps(comps: &[Value], cls: &str) -> Option<Desc> {
        let by: HashMap<&str, &Value> = comps.iter().filter(|c| truthy(Some(c))).map(|c| (jstr(c, "__struct__"), c)).collect();
        let mut out = Self::describe_kind(&by, cls)?;
        if matches!(out.kind, "seat" | "turret") {
            let ac = by.get("SAnimationControllerParams");
            let att = by.get("SAttachableComponentParams").and_then(|a| a.get("AttachDef"));
            let seat = by.get("SCItemSeatParams");
            out.adb = s_of(ac.and_then(|a| a.get("AnimationDatabase")));
            out.mtag = s_of(att.and_then(|a| a.get("mannequinTags")).and_then(|m| m.get("mannequinClassTag")));
            out.trans = s_of(seat.and_then(|s| s.get("transitionTemplate")));
            out.ctl = s_of(seat.and_then(|s| s.get("controlTemplate")));
        }
        Some(out)
    }

    fn describe_kind(comps: &HashMap<&str, &Value>, cls: &str) -> Option<Desc> {
        let inter = comps.get("SEntityInteractableParams").copied();
        let mut radius: Option<f64> = None;
        if let Some(i) = inter {
            let mut ips = vec![];
            find_struct(i, "SInteractionPointParams", &mut ips, 0);
            radius = ips.first().and_then(|ip| opt_num(ip.get("Radius")));
        }
        if let Some(lad) = comps.get("LadderComponentParams") {
            let h = jnum(lad, "height");
            return Some(Desc {
                kind: "ladder",
                height: if h != 0.0 { h } else { 8.0 },
                approach_top: jnum(lad, "approachAngleAtTop"),
                back: truthy(lad.get("directionAtTopIncludesBack")),
                left: truthy(lad.get("directionAtTopIncludesLeft")),
                right: truthy(lad.get("directionAtTopIncludesRight")),
                radius: 1.5,
                ..Default::default()
            });
        }
        if let Some(door) = comps.get("SCItemDoorParams") {
            let speed = jnum(door, "DoorAnimationSpeed");
            return Some(Desc {
                kind: "door",
                open: truthy(door.get("IsOpened")),
                speed: if speed != 0.0 { speed } else { 1.0 },
                auto_close: if truthy(door.get("UseAutoCloseDelay")) { jnum(door, "AutoCloseDelay") } else { 0.0 },
                radius: num_or(radius.map(|r| json!(r)).as_ref(), 1.6),
                ..Default::default()
            });
        }
        let seat = comps.get("SCItemSeatParams");
        if let Some(seat) = seat {
            if let Some(aa) = seat.get("actorAttachment").filter(|a| a.is_object()) {
                if truthy(aa.get("boneName")) {
                    let kind = if comps.contains_key("SCItemTurretParams") { "turret" } else { "seat" };
                    let bone = jstr(aa, "boneName").to_string();
                    let mut out = Desc {
                        kind,
                        bone: bone.clone(),
                        source: "seat",
                        seat_type: s_of(seat.get("seatType")),
                        yaw: [opt_num(seat.get("minYaw")), opt_num(seat.get("maxYaw"))],
                        pitch: [opt_num(seat.get("minPitch")), opt_num(seat.get("maxPitch"))],
                        radius: num_or(radius.map(|r| json!(r)).as_ref(), 2.0),
                        ..Default::default()
                    };
                    if s_of(aa.get("stance")).to_lowercase().contains("prone") {
                        out.bed = Some(BedInfo { modes: vec!["lie"], main: "lie", lie: bone, sit: None, inferred: false });
                    } else if kind == "seat" && res().bed_class.is_match(cls) {
                        out.bed = Some(BedInfo { modes: vec!["sit", "lie"], main: "sit", lie: bone.clone(), sit: Some(bone), inferred: true });
                    }
                    return Some(out);
                }
            }
        }
        if let Some(tur) = comps.get("SCItemTurretParams") {
            if truthy(tur.get("remoteTurret")) {
                return Some(Desc { kind: "aim", radius: num_or(radius.map(|r| json!(r)).as_ref(), 2.0), ..Default::default() });
            }
        }
        if let Some(use_) = comps.get("SActorUsableParams") {
            let mut slots = vec![];
            find_struct(use_, "PointAlignmentSlot", &mut slots, 0);
            let sit = slots.iter().copied().find(|sl| s_of(sl.get("stance")).to_lowercase().contains("seat") && truthy(sl.get("helperName")));
            let mut lie = slots.iter().copied().find(|sl| s_of(sl.get("stance")).to_lowercase().contains("prone") && truthy(sl.get("helperName")));
            if lie.is_none() {
                lie = slots.iter().copied().find(|x| {
                    truthy(x.get("helperName")) && !s_of(x.get("stance")).to_lowercase().contains("stand") && interaction_names(x).iter().any(|n| n == "LieDown")
                });
            }
            let sl = sit.or(lie);
            if let Some(sl) = sl {
                let vp = sl.get("viewParams").cloned().unwrap_or(json!({}));
                let mut ips = vec![];
                find_struct(sl, "SInteractionPointParams", &mut ips, 0);
                let ipr = ips.first().and_then(|ip| opt_num(ip.get("Radius")));
                let mut out = Desc {
                    kind: "seat",
                    bone: jstr(sl, "helperName").to_string(),
                    source: "usable",
                    yaw: [opt_num(vp.get("viewLimitYawMin")), opt_num(vp.get("viewLimitYawMax"))],
                    pitch: [opt_num(vp.get("viewLimitPitchMin")), opt_num(vp.get("viewLimitPitchMax"))],
                    radius: {
                        let a = ipr.filter(|v| *v != 0.0);
                        a.or(radius.filter(|v| *v != 0.0)).unwrap_or(2.0)
                    },
                    ..Default::default()
                };
                if let Some(l) = lie {
                    out.bed = Some(bed_info(&slots, sit, l));
                }
                return Some(out);
            }
        }
        if let Some(i) = inter {
            return panel_desc(i, cls, radius);
        }
        None
    }

    fn station(&self, out: &mut Desc, comps: &[Value], cls: &str) {
        let by: HashMap<&str, &Value> = comps.iter().filter(|c| truthy(Some(c))).map(|c| (jstr(c, "__struct__"), c)).collect();
        let empty = json!({});
        let seat = by.get("SCItemSeatParams").copied().unwrap_or(&empty);
        let tur = by.get("SCItemTurretParams").copied();
        let mut modes = None;
        if let Some(Value::Array(am)) = seat.get("availableOperatorModes") {
            if am.len() == 3 && am[1].as_f64() != Some(4294967295.0) {
                if let (Some(g), Some(d)) = (am[2].as_str(), self.dcb()) {
                    if let Some(gi) = self.items.guid_record(g) {
                        if let Some(rec) = d.record(gi).ok().and_then(|r| d.record_data(&r, 2).ok()) {
                            modes = station::operator_modes(&rec.to_json());
                        }
                    }
                }
            }
        }
        out.modes = modes;
        out.ctl = s_of(seat.get("controlTemplate"));
        if out.kind != "aim" {
            let av: Vec<String> = out.modes.as_ref().map(|m| m.av.clone()).unwrap_or_default();
            let def = out.modes.as_ref().map(|m| m.def.clone()).unwrap_or_default();
            out.subkind = station::subkind(cls, out.kind, &def, &av, tur.is_some(), out.bed.is_some(), out.source);
            let inter = by.get("SEntityInteractableParams").and_then(|i| i.get("Interactable")).cloned().unwrap_or(json!({}));
            let shared = inter.get("SharedInteractions").and_then(Value::as_array).cloned().unwrap_or_default();
            out.cmds = station::vehicle_commands(&shared);
            out.btns = station::dashboard_buttons(by.get("SCItemSeatDashboardParams").copied().unwrap_or(&Value::Null));
        }
        out.turret = tur.and_then(station::turret_spec);
    }

    // ------------------------------------------------------------------ geometry
    /// Parsed node table of an archive geometry (the overlay when it exists), cached.
    pub fn cgf(&self, src: &GeoSource, name: &str) -> Option<Arc<Cgf>> {
        if let Some(v) = self.c.lock().unwrap().cgf.get(name) {
            return v.clone();
        }
        let got = src.read(name).and_then(|r| r.ok()).and_then(|b| ivo::parse_cgf(&b, false).ok()).map(Arc::new);
        self.c.lock().unwrap().cgf.insert(name.to_string(), got.clone());
        got
    }

    /// Archive entry name for a path as written in a record, or None.
    pub fn archive_name(&self, path: &str) -> Option<String> {
        let p = path.replace('\\', "/").to_lowercase();
        let p = p.trim_start_matches('/').to_string();
        let ext = p.rfind('.').map(|i| p[i..].to_string())?;
        let idx = self.p4k.lower_index(&ext);
        for k in [p.clone(), format!("data/{}", p)] {
            if let Some(&i) = idx.map.get(&k) {
                return Some(self.p4k.entries[i].name.to_string());
            }
        }
        None
    }

    fn read_name(&self, name: &str) -> Option<Vec<u8>> {
        self.p4k.get(name).and_then(|e| self.p4k.read(e).ok())
    }

    /// (min, max) of a model's mesh header box in model space (EMPIRICAL: single-mesh .cgfm / .cgam), or None.
    pub fn model_box(&self, geo_name: &str) -> Option<[V3; 2]> {
        let stem = &geo_name[..geo_name.rfind('.').unwrap_or(geo_name.len())];
        for ext in companion_exts(geo_name) {
            if let Some(n) = self.archive_name(&format!("{}{}", stem, ext)) {
                let data = match self.read_name(&n) {
                    Some(d) => d,
                    None => return None,
                };
                let ms = ivo::parse_cgfm(&data).ok()?;
                let m = ms.first()?;
                if m.bbox_min.iter().chain(m.bbox_max.iter()).all(|v| v.abs() < 1e5) {
                    return Some([m.bbox_min, m.bbox_max]);
                }
                return None;
            }
        }
        None
    }

    // ------------------------------------------------------------------ clips
    fn dba_db(&self, n: &str) -> Option<Arc<Dba>> {
        if let Some(v) = self.c.lock().unwrap().dba.get(n) {
            return v.clone();
        }
        let got = self.read_name(n).and_then(|b| dba::parse_dba(&b).ok()).map(Arc::new);
        self.c.lock().unwrap().dba.insert(n.to_string(), got.clone());
        got
    }

    fn chr_dbs(&self, info: &dba::ChrParams) -> Vec<Arc<Dba>> {
        info.tracks.iter().filter_map(|t| self.archive_name(t)).filter_map(|n| self.dba_db(&n)).collect()
    }

    /// (open Clip, close Clip) of a door model from its .chrparams and the .dba it names.
    pub fn clips(&self, src: &GeoSource, geo_name: &str) -> (Option<Arc<Clip>>, Option<Arc<Clip>>) {
        if let Some(v) = self.c.lock().unwrap().chr.get(geo_name) {
            return v.clone();
        }
        let mut res = (None, None);
        let stem = &geo_name[..geo_name.rfind('.').unwrap_or(geo_name.len())];
        if let Some(cp) = self.archive_name(&format!("{}.chrparams", stem)) {
            if let Some(info) = self.read_name(&cp).and_then(|b| dba::parse_chrparams(&b).ok()) {
                let dbs = self.chr_dbs(&info);
                // EMPIRICAL (Tiburon blast doors): some doors name no .dba, their door_open / door_close are stand-alone .caf files
                let look = |a: &str| dba::clip_for(&info, &dbs, a).or_else(|| self.clip_named(src, geo_name, a, ""));
                let pick = |word: &str| -> Option<Arc<Clip>> {
                    let mut names: Vec<&String> = info.anims.iter().map(|(k, _)| k).collect();
                    names.sort_by_key(|a| (**a != format!("door_{}", word), (*a).clone()));
                    names.into_iter().filter(|a| a.contains(word)).find_map(|a| look(a))
                };
                res = (pick("open"), pick("close"));
            }
        }
        self.c.lock().unwrap().chr.insert(geo_name.to_string(), res.clone());
        res
    }

    /// How to split a door model into its moving parts: None when it has no animation.
    pub fn door_groups(&self, src: &GeoSource, geo_name: &str) -> Option<Arc<Dg>> {
        if let Some(v) = self.c.lock().unwrap().groups.get(geo_name) {
            return v.clone();
        }
        let cg = self.cgf(src, geo_name);
        let (opn, cls_) = self.clips(src, geo_name);
        let res = match (cg, opn) {
            (Some(cg), Some(o)) if !cg.nodes.is_empty() && !o.bones.is_empty() => split_door(&cg.nodes, Some(&o), cls_.as_deref(), true, None).map(Arc::new),
            _ => None,
        };
        self.c.lock().unwrap().groups.insert(geo_name.to_string(), res.clone());
        res
    }

    fn adb_opts(&self, path: &str) -> Arc<Vec<adb::AdbOption>> {
        if let Some(v) = self.c.lock().unwrap().adbs.get(path) {
            return v.clone();
        }
        let rel = path.replace('\\', "/");
        let rel = rel.trim_start_matches('/');
        let name = self.archive_name(rel).or_else(|| self.archive_name(&format!("animations/mannequin/adb/{}", rel)));
        let opts = name.and_then(|n| self.read_name(&n)).and_then(|b| adb::parse_adb(&b).ok()).unwrap_or_default();
        let a = Arc::new(opts);
        self.c.lock().unwrap().adbs.insert(path.to_string(), a.clone());
        a
    }

    /// The Clip the model's .chrparams maps animation name `anim` to: an entry of a .dba it lists, else a stand-alone .caf in the folder
    /// it names, else the clip of that file name found in ANY database of the archive (`clip_anywhere`).
    pub fn clip_named(&self, src: &GeoSource, geo_name: &str, anim: &str, hint: &str) -> Option<Arc<Clip>> {
        let key = (geo_name.to_string(), anim.to_string(), hint.to_string());
        if let Some(v) = self.c.lock().unwrap().clip.get(&key) {
            return v.clone();
        }
        let mut res: Option<Arc<Clip>> = None;
        let mut f: Option<String> = None;
        let stem = &geo_name[..geo_name.rfind('.').unwrap_or(geo_name.len())];
        let cp = self.archive_name(&format!("{}.chrparams", stem));
        if let Some(cp) = &cp {
            if let Some(mut info) = self.read_name(cp).and_then(|b| dba::parse_chrparams(&b).ok()) {
                let dbs = self.chr_dbs(&info);
                let al = anim.to_lowercase();
                f = info.anim(&al).map(str::to_owned);
                if f.is_none() && info.anim("*").is_some() {
                    // EMPIRICAL: <Animation name="*" path="*/*.caf"/> = any name is <folder>/<name>.caf
                    f = Some(format!("{}.caf", al));
                    info.anims.push((al.clone(), f.clone().unwrap()));
                }
                res = dba::clip_for(&info, &dbs, anim);
                if res.is_none() {
                    if let Some(ff) = &f {
                        let p = if ff.contains('/') { ff.clone() } else { format!("{}/{}", info.path.trim_end_matches('/'), ff) };
                        if let Some(n) = self.archive_name(&p) {
                            if n.to_lowercase().ends_with(".caf") {
                                res = self.read_name(&n).and_then(|b| dba::parse_caf(&b, &n).ok()).map(Arc::new);
                            }
                        }
                    }
                }
            }
        }
        if res.is_none() {
            res = self.clip_anywhere(src, geo_name, anim, if cp.is_some() { f.as_deref() } else { None }, hint);
        }
        self.c.lock().unwrap().clip.insert(key, res.clone());
        res
    }

    fn clip_index(&self) -> &HashMap<String, Vec<(String, String)>> {
        self.clip_idx.get_or_init(|| {
            let mut idx: HashMap<String, Vec<(String, String)>> = HashMap::new();
            let mut names: Vec<&str> = self.p4k.entries.iter().map(|e| e.name.as_ref()).filter(|n| n.to_lowercase().ends_with(".dba")).collect();
            names.sort_unstable();
            for n in names {
                if let Some(cl) = self.read_name(n).and_then(|b| dba::dba_names(&b).ok()) {
                    for c in cl {
                        idx.entry(c.rsplit('/').next().unwrap().to_string()).or_default().push((c.clone(), n.to_string()));
                    }
                }
            }
            idx
        })
    }

    const CLIP_FOLDER_NOISE: [&'static str; 6] = ["animations", "spaceships", "ships", "seats", "stations", "interior"];

    fn clip_anywhere(&self, src: &GeoSource, geo_name: &str, anim: &str, fname: Option<&str>, hint: &str) -> Option<Arc<Clip>> {
        let base = fname.map(str::to_owned).unwrap_or_else(|| format!("{}.caf", anim)).replace('\\', "/").rsplit('/').next().unwrap().to_lowercase();
        let cands = self.clip_index().get(&base)?;
        let low = format!("{} {}", geo_name, hint).to_lowercase();
        let words: HashSet<&str> = res().words.find_iter(&low).map(|m| m.as_str()).collect();
        let cg = self.cgf(src, geo_name);
        let names: Vec<&str> = cg.as_ref().map(|c| c.nodes.iter().map(|n| n.name.as_str()).collect()).unwrap_or_default();
        let mut scored: Vec<(i64, i64, Arc<Clip>)> = vec![];
        for (full, n) in cands {
            let Some(clip) = self.dba_db(n).and_then(|d| d.get(full).cloned()) else { continue };
            let ids: HashSet<u32> = clip.bones.iter().copied().collect();
            let hit = names.iter().filter(|nm| dba::bone_id(nm, &ids).is_some()).count() as i64;
            if !names.is_empty() && hit == 0 {
                continue;
            }
            let folder = &full[..full.rfind('/').unwrap_or(full.len())];
            let share = res().words.find_iter(folder).filter(|m| !Self::CLIP_FOLDER_NOISE.contains(&m.as_str()) && words.contains(m.as_str())).count() as i64;
            scored.push((share, hit, clip));
        }
        scored.sort_by(|a, b| (-a.0, -a.1).cmp(&(-b.0, -b.1)));
        if scored.is_empty() || (scored.len() > 1 && (scored[0].0, scored[0].1) == (scored[1].0, scored[1].1)) {
            return None;
        }
        let (share, hit, clip) = scored[0].clone();
        if share >= 1 || (scored.len() == 1 && hit >= 2) {
            Some(clip)
        } else {
            None
        }
    }

    /// [(seat port name, enter, exit)] of a seat access entity class (`SCItemSeatAccessParams`), [] for any other class.  Cached.
    pub fn access_skips(&self, cls: &str) -> Vec<(String, adb::Skip, adb::Skip)> {
        if let Some(v) = self.c.lock().unwrap().access.get(cls) {
            return v.clone();
        }
        let mut out = vec![];
        if !cls.is_empty() && self.ready() {
            let d = self.dcb().unwrap();
            if let Some(ids) = d.record_index().get(&format!("{}{}", RECORD, cls)) {
                let r: Option<()> = (|| {
                    let rec = d.record(ids[0]).ok()?;
                    let top = d.record_data(&rec, 0).ok()?;
                    let Some(Dv::List(comps)) = top.get("Components") else { return Some(()) };
                    for c in comps {
                        if let Dv::Strong(s, _) = c {
                            if *s != NULL_PTR && d.struct_name(*s as usize).ok()? == "SCItemSeatAccessParams" {
                                let j = d.deref(c.clone(), 0, 7).ok()?.to_json();
                                for e in j.get("entrances").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
                                    let port = s_of(e.get("seatConnection").and_then(|x| x.get("seatItemPort")).and_then(|x| x.get("itemPortName")));
                                    if !port.is_empty() {
                                        let st = |k: &str| -> adb::Skip {
                                            let x = e.get(k).cloned().unwrap_or(json!({}));
                                            let ss = s_of(x.get("skipState"));
                                            (if ss.is_empty() { "None".into() } else { ss }, s_of(x.get("fragmentTags")), s_of(x.get("deployRetractFragmentTags")))
                                        };
                                        out.push((port, st("enterState"), st("exitState")));
                                    }
                                }
                            }
                        }
                    }
                    Some(())
                })();
                if r.is_none() {
                    out.clear();
                }
            }
        }
        self.c.lock().unwrap().access.insert(cls.to_string(), out.clone());
        out
    }

    /// {'sit': [(state, Clip)], 'stand': [...]}: the clips of the engine's sit-down / stand-up state sequences that resolve for this model.
    pub fn seat_sequence_clips(&self, src: &GeoSource, geo_name: &str, desc: &Desc) -> (Vec<(String, Arc<Clip>)>, Vec<(String, Arc<Clip>)>) {
        if desc.adb.is_empty() {
            return (vec![], vec![]);
        }
        let (enter, exit) = match &desc.ent {
            Some((a, b)) => (a.clone(), b.clone()),
            None => (adb::enter_default(), adb::exit_default()),
        };
        let templates = vec![desc.trans.clone(), desc.ctl.clone()];
        let (sit_s, stand_s) = adb::seat_sequences(&self.adb_opts(&desc.adb), &desc.mtag, &templates, &enter, &exit, desc.ent.is_none());
        let hint = format!("{} {}", desc.adb, desc.mtag);
        let resolve = |seq: Vec<adb::StateOption>| -> Vec<(String, Arc<Clip>)> {
            let mut out = vec![];
            for o in seq {
                if let Some(c) = self.clip_named(src, geo_name, &o.anim, &hint) {
                    if !c.bones.is_empty() {
                        out.push((o.state.clone(), if o.speed < 0.0 { Arc::new(dba::reverse_clip(&c)) } else { c }));
                    }
                }
            }
            out
        };
        (resolve(sit_s), resolve(stand_s))
    }

    /// (sit Clip, stand Clip) of a seat model; a direction without any chair clip is the other one reversed.
    pub fn seat_clips(&self, src: &GeoSource, geo_name: &str, desc: &Desc) -> (Option<Arc<Clip>>, Option<Arc<Clip>>) {
        let (sit_s, stand_s) = self.seat_sequence_clips(src, geo_name, desc);
        let join = |items: Vec<(String, Arc<Clip>)>| -> Option<Arc<Clip>> {
            match items.len() {
                0 => None,
                1 => Some(items[0].1.clone()),
                _ => {
                    let clips: Vec<Arc<Clip>> = items.iter().map(|(_, c)| c.clone()).collect();
                    let name = items.iter().map(|(_, c)| c.name.rsplit('/').next().unwrap().to_string()).collect::<Vec<_>>().join("+");
                    Some(Arc::new(dba::concat_clips(&clips, &name)))
                }
            }
        };
        let (mut sit, mut stand) = (join(sit_s), join(stand_s));
        if sit.is_none() && stand.is_some() {
            sit = stand.as_ref().map(|c| Arc::new(dba::reverse_clip(c)));
        } else if stand.is_none() && sit.is_some() {
            stand = sit.as_ref().map(|c| Arc::new(dba::reverse_clip(c)));
        }
        (sit, stand)
    }

    /// Like `door_groups` for a seat model: its moving chair parts (sit-down sequence 'o', stand-up 'c') and a turret's joints.
    pub fn seat_groups(&self, src: &GeoSource, geo_name: &str, desc: &Desc) -> Option<Arc<Dg>> {
        let spec = desc.turret.as_ref();
        let key = format!(
            "{}|{}|{}|{}|{}|{:?}|{:?}",
            geo_name,
            desc.adb,
            desc.mtag,
            desc.trans,
            desc.ctl,
            desc.ent,
            spec.map(|s| s.joints.iter().map(|j| j.n.clone()).collect::<Vec<_>>())
        );
        if let Some(v) = self.c.lock().unwrap().seatg.get(&key) {
            return v.clone();
        }
        let cg = self.cgf(src, geo_name);
        let mut res = None;
        if let Some(cg) = cg.filter(|c| !c.nodes.is_empty()) {
            let (sit, stand) = self.seat_clips(src, geo_name, desc);
            let mut tj: Vec<ResolvedJoint> = vec![];
            if let Some(spec) = spec {
                let mut parents: HashMap<usize, usize> = HashMap::new();
                for n in &cg.nodes {
                    for &c in &n.children {
                        parents.insert(c as usize, n.index);
                    }
                }
                let names: HashMap<String, usize> = cg.nodes.iter().map(|n| (n.name.to_lowercase(), n.index)).collect();
                tj = station::resolve_joints(spec, &names, &parents);
            }
            let has = sit.as_ref().map_or(false, |s| !s.bones.is_empty());
            if has || !tj.is_empty() {
                res = split_door(&cg.nodes, if has { sit.as_deref() } else { None }, if has { stand.as_deref() } else { None }, false, if tj.is_empty() { None } else { Some(&tj) }).map(Arc::new);
            }
        }
        self.c.lock().unwrap().seatg.insert(key, res.clone());
        res
    }

    /// World position of a panel's interaction point: its `$IP_*` helper node (no triangles) when the model has one, else the origin.
    pub fn panel_point(&self, src: &GeoSource, geo_name: &str, matrix: &Mat) -> V3 {
        let cg = self.cgf(src, geo_name);
        let node = cg.as_ref().and_then(|c| c.nodes.iter().find(|n| n.name.to_lowercase().starts_with("$ip") && n.num_tris == 0));
        let w = match node {
            Some(n) => compose(matrix, &n.world),
            None => *matrix,
        };
        [w[3], w[7], w[11]]
    }

    /// The seat pose of a seat model placed with `matrix`, or None when the model lacks the sit helper.
    pub fn seat_point(&self, src: &GeoSource, geo_name: &str, desc: &Desc, matrix: &Mat, dg: Option<&Dg>) -> Option<SeatPoint> {
        let cg = self.cgf(src, geo_name)?;
        let nodes = &cg.nodes;
        let bone = desc.bone.to_lowercase();
        let node = nodes.iter().rev().find(|n| n.name.to_lowercase() == bone)?;
        let sit_w = match dg {
            Some(g) => node_world(nodes, node.index, &g.joints, true, g.seconds),
            None => node.world,
        };
        let w = compose(matrix, &sit_w);
        let fwd = [w[1], w[5], w[9]];
        let yaw = fwd[1].atan2(fwd[0]);
        let up = [w[2], w[6], w[10]];
        let eye = [w[3] + up[0] * SEAT_EYE, w[7] + up[1] * SEAT_EYE, w[11] + up[2] * SEAT_EYE];
        let ex_re = &res().exit;
        let ex = nodes
            .iter()
            .find(|n| ex_re.is_match(&n.name) && n.num_tris == 0 && n.name.to_lowercase().contains("end"))
            .or_else(|| nodes.iter().find(|n| ex_re.is_match(&n.name) && n.num_tris == 0));
        let (exit, exit_yaw);
        if let Some(ex) = ex {
            let e = compose(
                matrix,
                &match dg {
                    Some(g) => node_world(nodes, ex.index, &g.joints, false, g.seconds_close),
                    None => ex.world,
                },
            );
            exit = [e[3], e[7], e[11]];
            exit_yaw = Some(e[5].atan2(e[1]));
        } else {
            let mut ln = fwd[0].hypot(fwd[1]);
            if ln == 0.0 {
                ln = 1.0;
            }
            exit = [w[3] - fwd[0] / ln * EXIT_BACK, w[7] - fwd[1] / ln * EXIT_BACK, w[11]];
            exit_yaw = None;
        }
        let pos = match dg {
            Some(g) => {
                let r0 = compose(matrix, &node_world(nodes, node.index, &g.joints, true, 0.0));
                [r0[3], r0[7], r0[11]]
            }
            None => [w[3], w[7], w[11]],
        };
        let mut out = SeatPoint { pos, seated: [w[3], w[7], w[11]], eye, yaw, exit, exit_yaw, node: node.name.clone(), exit_node: ex.map(|e| e.name.clone()), lie: None, eyem: None, eyeg: None };
        if let Some(bd) = &desc.bed {
            out.lie = bed::lie_feed(nodes, bd, matrix, exit);
        }
        if let Some(g) = dg {
            let r = &node.world;
            out.eyem = Some([r[3] + r[2] * SEAT_EYE, r[7] + r[6] * SEAT_EYE, r[11] + r[10] * SEAT_EYE]);
            out.eyeg = g.groups.iter().position(|gr| gr.nodes.contains(&node.index) && gr.j.is_some());
        }
        Some(out)
    }
}

/// A seat pose (`seat_point`).
#[derive(Debug, Clone)]
pub struct SeatPoint {
    pub pos: V3,
    pub seated: V3,
    pub eye: V3,
    pub yaw: f64,
    pub exit: V3,
    pub exit_yaw: Option<f64>,
    pub node: String,
    pub exit_node: Option<String>,
    pub lie: Option<Value>,
    pub eyem: Option<V3>,
    pub eyeg: Option<usize>,
}

/// Distance from the sit helper `bone` to the nearest foot IK helper at the start and at the end of the sit-down sequence of `dg`.
pub fn foot_distances(nodes: &[Node], bone: &str, dg: &Dg) -> Option<(f64, f64)> {
    let sit = nodes.iter().rev().find(|n| n.name.to_lowercase() == bone.to_lowercase())?;
    let feet: Vec<&Node> = nodes.iter().filter(|n| n.num_tris == 0 && res().foot.is_match(&n.name)).collect();
    if feet.is_empty() {
        return None;
    }
    let mut out = vec![];
    for t in [0.0, dg.seconds] {
        let w = node_world(nodes, sit.index, &dg.joints, true, t);
        let d = feet
            .iter()
            .map(|f| {
                let fw = node_world(nodes, f.index, &dg.joints, true, t);
                dist([w[3], w[7], w[11]], [fw[3], fw[7], fw[11]])
            })
            .fold(f64::INFINITY, f64::min);
        out.push(d);
    }
    Some((out[0], out[1]))
}

#[allow(dead_code)]
fn _r3(v: V3) -> Vec<f64> {
    r3(v, 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_set_order() {
        // expected values printed by CPython 3.14: list(set) after the same adds
        assert_eq!(py_set_order(&[20, 3, 15, 9, 40, 7, 33, 12, 1, 25, 18, 64, 5, 100, 8]), vec![64, 33, 1, 3, 100, 5, 7, 40, 9, 8, 12, 15, 18, 20, 25]);
        assert_eq!(py_set_order(&[3, 1, 2]), vec![1, 2, 3]);
    }

    #[test]
    fn inverse_roundtrip() {
        let m = quat_scale_pos_matrix([0.9, 0.1, 0.3, 0.2], [1.0; 3], [1.0, 2.0, 3.0]);
        let i = compose(&m, &inv34(&m));
        for (a, b) in i.iter().zip(IDENT.iter()) {
            assert!((a - b).abs() < 1e-9);
        }
    }

    #[test]
    fn quat_roundtrip() {
        let q = [0.1f64, 0.3, 0.2, 0.9];
        let n = (q.iter().map(|v| v * v).sum::<f64>()).sqrt();
        let q = [q[0] / n, q[1] / n, q[2] / n, q[3] / n];
        let m = local_matrix(q, [0.0; 3]);
        let r = mat_to_quat(&m);
        let dot: f64 = q.iter().zip(r.iter()).map(|(a, b)| a * b).sum();
        assert!(dot.abs() > 0.999999);
    }
}
