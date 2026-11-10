//! Where a person lies on a bed: placement of the lying body and its eye from the bed model's own data.
//!
//! Evidence (EMPIRICAL = observed on the live archive, INFERRED = a reading): the 188 usable bed classes name a
//! `$anchor_bed` helper on the mattress with +Y from head to feet; the lying idle clip's World root heading +Y has the head at
//! (-0.01, -0.81, 0.16); beds without lying data fall back to `$slot_mattress` / `$slot_pillow`, the mattress slab of the mesh, `bed_block`,
//! then the sit helper.  The lying clip table and clip poses at the end of this file need the character skeleton and its clips; they
//! are used by the bed clip route, not by the zone feed.

use std::collections::HashMap;

use p4k_formats::ivo::Node;
use p4k_formats::socpak::compose;
use serde_json::{json, Value};

pub const LIE_HEAD: [f64; 3] = [-0.005, -0.811, 0.159];
pub const LIE_EYE: [f64; 3] = [-0.005, -0.832, 0.237];
pub const LIE_LOOK: [f64; 3] = [0.005, 0.251, 0.968];
pub const LIE_PITCH: [f64; 2] = [20.0, 85.0];
pub const LIE_YAW: [f64; 2] = [-60.0, 60.0];
pub const SLAB_THIN: f64 = 0.35;
pub const SLAB_LONG: [f64; 2] = [1.5, 3.2];
pub const SLAB_WIDE: [f64; 2] = [0.45, 1.8];

type V3 = [f64; 3];

/// (position, heading) of a 3x4 matrix: heading = azimuth of its +Y axis (radians, 0 = +X).
pub fn frame_of(w: &[f64; 12]) -> (V3, f64) {
    ([w[3], w[7], w[11]], w[5].atan2(w[1]))
}

pub fn rot2(v: [f64; 2], a: f64) -> [f64; 2] {
    let (c, s) = (a.cos(), a.sin());
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c]
}

pub fn to_world(pos: V3, heading: f64, local: V3) -> V3 {
    let r = rot2([local[0], local[1]], heading - std::f64::consts::FRAC_PI_2);
    [pos[0] + r[0], pos[1] + r[1], pos[2] + local[2]]
}

pub fn to_local(pos: V3, heading: f64, p: V3) -> V3 {
    let r = rot2([p[0] - pos[0], p[1] - pos[1]], -(heading - std::f64::consts::FRAC_PI_2));
    [r[0], r[1], p[2] - pos[2]]
}

fn col(w: &[f64; 12], j: usize) -> V3 {
    [w[j], w[4 + j], w[8 + j]]
}

fn xf(m: &[f64; 12], p: V3) -> V3 {
    [m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3], m[4] * p[0] + m[5] * p[1] + m[6] * p[2] + m[7], m[8] * p[0] + m[9] * p[1] + m[10] * p[2] + m[11]]
}

/// A mattress box: centre, top z, unit long axis (x, y), long / wide extents.
#[derive(Debug, Clone, Copy)]
pub struct Slab {
    pub c: V3,
    pub top: f64,
    pub ax: [f64; 2],
    pub long: f64,
    pub wide: f64,
}

/// The mattress of a bed model: the largest flat box among the mesh nodes (the one whose top face is closest to `near`, among those that
/// lie under it, for a bunk with two mattresses), or None.
pub fn mattress_slab(nodes: &[Node], near: Option<V3>, loose: bool) -> Option<Slab> {
    let mut best: Option<Slab> = None;
    let mut found: Vec<Slab> = vec![];
    for n in nodes {
        if n.num_tris == 0 {
            continue;
        }
        let b = n.bbox;
        let (mut ext, mut axes) = ([0.0; 3], [[0.0; 3]; 3]);
        for j in 0..3 {
            let c = col(&n.world, j);
            let mut ln = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
            if ln == 0.0 {
                ln = 1.0;
            }
            ext[j] = (b[3 + j] - b[j]) * ln;
            axes[j] = [c[0] / ln, c[1] / ln, c[2] / ln];
        }
        let mut order = [0usize, 1, 2];
        order.sort_by(|&a, &b| ext[a].partial_cmp(&ext[b]).unwrap_or(std::cmp::Ordering::Equal));
        let (thin, wide, long_) = (order[0], order[1], order[2]);
        let (lo_long, lo_wide) = if loose { (1.0, 0.3) } else { (SLAB_LONG[0], SLAB_WIDE[0]) };
        if ext[thin] > SLAB_THIN || !(lo_long <= ext[long_] && ext[long_] <= SLAB_LONG[1]) || !(lo_wide <= ext[wide] && ext[wide] <= SLAB_WIDE[1]) {
            continue;
        }
        if axes[thin][2].abs() < 0.9 || axes[long_][2].abs() > 0.35 {
            continue;
        }
        let mid = [(b[0] + b[3]) / 2.0, (b[1] + b[4]) / 2.0, (b[2] + b[5]) / 2.0];
        let w = &n.world;
        let c = [
            w[0] * mid[0] + w[1] * mid[1] + w[2] * mid[2] + w[3],
            w[4] * mid[0] + w[5] * mid[1] + w[6] * mid[2] + w[7],
            w[8] * mid[0] + w[9] * mid[1] + w[10] * mid[2] + w[11],
        ];
        let a = axes[long_];
        let mut ln = a[0].hypot(a[1]);
        if ln == 0.0 {
            ln = 1.0;
        }
        let slab = Slab { c, top: c[2] + ext[thin] / 2.0, ax: [a[0] / ln, a[1] / ln], long: ext[long_], wide: ext[wide] };
        found.push(slab);
        if best.map_or(true, |bs| slab.long * slab.wide > bs.long * bs.wide) {
            best = Some(slab);
        }
    }
    if let (Some(near), false) = (near, found.is_empty()) {
        let under = |sl: &Slab| {
            let d0 = (near[0] - sl.c[0]) * sl.ax[0] + (near[1] - sl.c[1]) * sl.ax[1];
            let d1 = -(near[0] - sl.c[0]) * sl.ax[1] + (near[1] - sl.c[1]) * sl.ax[0];
            d0.abs() <= sl.long / 2.0 + 0.3 && d1.abs() <= sl.wide / 2.0 + 0.3
        };
        let mut cand: Option<&Slab> = None;
        for sl in found.iter().filter(|s| under(s)) {
            if cand.map_or(true, |c| (sl.top - near[2]).abs() < (c.top - near[2]).abs()) {
                cand = Some(sl);
            }
        }
        if let Some(c) = cand {
            return Some(*c);
        }
    }
    best
}

/// Last-resort bed volume: the mesh node whose axis-aligned box (model space) is 1.5 to 3.2 m long, 0.45 to 1.8 m wide and at most 1 m high.
pub fn bed_block(nodes: &[Node]) -> Option<Slab> {
    let mut best: Option<Slab> = None;
    for n in nodes {
        if n.num_tris == 0 {
            continue;
        }
        let b = n.bbox;
        let (mut lo, mut hi) = ([1e30; 3], [-1e30; 3]);
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
        let ext = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
        let mut horiz = [ext[0], ext[1]];
        horiz.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if ext[2] > 1.0 || !(SLAB_LONG[0] <= horiz[1] && horiz[1] <= SLAB_LONG[1]) || !(SLAB_WIDE[0] <= horiz[0] && horiz[0] <= SLAB_WIDE[1]) {
            continue;
        }
        let c = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
        let slab = Slab { c, top: hi[2], ax: if ext[0] >= ext[1] { [1.0, 0.0] } else { [0.0, 1.0] }, long: horiz[1], wide: horiz[0] };
        if best.map_or(true, |bs| slab.long * slab.wide > bs.long * bs.wide) {
            best = Some(slab);
        }
    }
    best
}

/// Lying frame (position, heading) of a mattress slab: the middle of its top face; the head is the end farther from `entry_xy`.
pub fn slab_frame(slab: &Slab, entry_xy: [f64; 2]) -> (V3, f64) {
    let ax = slab.ax;
    let d = (entry_xy[0] - slab.c[0]) * ax[0] + (entry_xy[1] - slab.c[1]) * ax[1];
    let sign = if d >= 0.0 { 1.0 } else { -1.0 };
    ([slab.c[0], slab.c[1], slab.top], (ax[1] * sign).atan2(ax[0] * sign))
}

/// The lying pose of a body whose World root is at `pos` heading `heading`.
pub struct LiePose {
    pub eye: V3,
    pub yaw: f64,
    pub pitch: f64,
}

pub fn lie_pose(pos: V3, heading: f64) -> LiePose {
    LiePose { eye: to_world(pos, heading, LIE_EYE), yaw: heading, pitch: LIE_LOOK[2].clamp(-1.0, 1.0).asin() }
}

/// 'l' / 'r' / 'b': from which side of the lying body the point `p` enters.
pub fn entry_side(pos: V3, heading: f64, p: V3) -> &'static str {
    let l = to_local(pos, heading, p);
    if l[1] > 0.6 && l[1] > l[0].abs() + 0.3 {
        "b"
    } else if l[0] < 0.0 {
        "l"
    } else {
        "r"
    }
}

pub fn exit_beside(pos: V3, heading: f64, p: V3, half_wide: f64) -> V3 {
    let margin = 0.6;
    let l = to_local(pos, heading, p);
    let side = if l[0] < 0.0 { -1.0 } else { 1.0 };
    let mut out = to_world(pos, heading, [side * (half_wide + margin), 0.0, 0.0]);
    out[2] = p[2];
    out
}

pub fn exit_point(pos: V3, heading: f64, p: V3, half_wide: f64) -> V3 {
    let l = to_local(pos, heading, p);
    if 0.25 <= l[0].abs() && l[0].abs() <= 2.2 && l[1].abs() <= 1.6 {
        return p;
    }
    exit_beside(pos, heading, p, half_wide)
}

pub fn local_box(b: &[V3; 2], pos: V3, heading: f64) -> [V3; 2] {
    let (mut lo, mut hi) = ([1e30; 3], [-1e30; 3]);
    for x in [b[0][0], b[1][0]] {
        for y in [b[0][1], b[1][1]] {
            for z in [b[0][2], b[1][2]] {
                let p = to_local(pos, heading, [x, y, z]);
                for k in 0..3 {
                    lo[k] = f64::min(lo[k], p[k]);
                    hi[k] = f64::max(hi[k], p[k]);
                }
            }
        }
    }
    [lo, hi]
}

/// (min, max) in world axes of the mesh nodes' boxes, or None when the model has no mesh.
pub fn render_box(nodes: &[Node], matrix: &[f64; 12]) -> Option<[V3; 2]> {
    let (mut lo, mut hi) = ([1e30; 3], [-1e30; 3]);
    for n in nodes {
        if n.num_tris == 0 {
            continue;
        }
        let b = n.bbox;
        let w = compose(matrix, &n.world);
        for x in [b[0], b[3]] {
            for y in [b[1], b[4]] {
                for z in [b[2], b[5]] {
                    let p = xf(&w, [x, y, z]);
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

/// A slab in world axes (instance matrix applied; its scale applies to the height).
fn world_slab(slab: Option<Slab>, m: &[f64; 12]) -> Option<(V3, f64, [f64; 2])> {
    let s = slab?;
    let c = xf(m, s.c);
    let sc = (m[8] * m[8] + m[9] * m[9] + m[10] * m[10]).sqrt();
    Some((c, c[2] + (s.top - s.c[2]) * sc, rot2(s.ax, m[4].atan2(m[0]))))
}

/// The bed description of a seat: `modes`, `main`, `lie` (helper), `sit` (helper or None), `inferred`.
#[derive(Debug, Clone, PartialEq)]
pub struct BedInfo {
    pub modes: Vec<&'static str>,
    pub main: &'static str,
    pub lie: String,
    pub sit: Option<String>,
    pub inferred: bool,
}

impl BedInfo {
    pub fn to_json(&self) -> Value {
        let mut v = json!({"modes": self.modes, "main": self.main, "lie": self.lie});
        if self.sit.is_some() || !self.inferred {
            v["sit"] = json!(self.sit);
        }
        if self.inferred {
            v["sit"] = json!(self.sit);
            v["inferred"] = json!(true);
        }
        v
    }
}

/// The `lie` entry of a bed item of the feed (world axes, instance `matrix` applied) or None when no lying frame can be found.
/// `exit_` = the seat's own exit point.  Values are rounded by `rounded` for the feed.
pub fn lie_feed(nodes: &[Node], bed: &BedInfo, matrix: &[f64; 12], exit_: V3) -> Option<Value> {
    let by: HashMap<String, &Node> = nodes.iter().map(|n| (n.name.to_lowercase(), n)).collect();
    let name = bed.lie.to_lowercase();
    let anchor = by.get(&name).copied();
    let near = anchor.map(|a| [a.world[3], a.world[7], a.world[11]]);
    let mut sw = world_slab(mattress_slab(nodes, near, false), matrix);
    let pil = by.get("$slot_pillow").copied();
    let mat = by.get("$slot_mattress").copied();
    let ent = by.get("$ip_enter").or_else(|| by.get("ip_enter")).copied();
    let ew = ent.map(|e| compose(matrix, &e.world));
    let entry_xy = match ew {
        Some(w) => [w[3], w[7]],
        None => [exit_[0], exit_[1]],
    };
    let (pos, heading, mode);
    if anchor.is_some() && !bed.inferred && name.starts_with("$anchor_bed") {
        let (p, h) = frame_of(&compose(matrix, &anchor.unwrap().world));
        pos = p;
        heading = h;
        mode = "anchor";
    } else if let (Some(pil), Some(mat)) = (pil, mat) {
        let a = compose(matrix, &mat.world);
        let b = compose(matrix, &pil.world);
        pos = [a[3], a[7], a[11]];
        heading = (a[7] - b[7]).atan2(a[3] - b[3]);
        mode = "slots";
    } else if sw.is_some() || world_slab(mattress_slab(nodes, None, true).or_else(|| bed_block(nodes)), matrix).is_some() {
        if sw.is_none() {
            sw = world_slab(mattress_slab(nodes, None, true).or_else(|| bed_block(nodes)), matrix);
        }
        let (c, top, ax) = sw.unwrap();
        let s = Slab { c, top, ax, long: 0.0, wide: 0.0 };
        let (p, h) = slab_frame(&s, entry_xy);
        pos = p;
        heading = h;
        mode = "slab";
    } else if let Some(a) = anchor {
        let (p, h) = frame_of(&compose(matrix, &a.world));
        pos = p;
        heading = h;
        mode = "sit";
    } else {
        return None;
    }
    let pose = lie_pose(pos, heading);
    let mut out = json!({"mode": mode, "eye": pose.eye, "yaw": pose.yaw, "pitch": pose.pitch,
                         "lim": {"yaw": LIE_YAW, "pitch": LIE_PITCH}, "root": pos, "heading": heading});
    let mut hw = 0.5;
    if let Some(bx) = render_box(nodes, matrix) {
        let lb = local_box(&bx, pos, heading);
        out["box"] = json!([lb[0].iter().map(|v| round_n(*v, 3)).collect::<Vec<_>>(), lb[1].iter().map(|v| round_n(*v, 3)).collect::<Vec<_>>()]);
        hw = (0.35f64).max((1.2f64).min((lb[1][0] - lb[0][0]) / 2.0));
    }
    if let Some((_, top, _)) = sw {
        out["surf"] = json!(round_n(top - pos[2], 3));
    }
    out["exit"] = json!(exit_point(pos, heading, exit_, hw));
    out["side"] = json!(entry_side(pos, heading, exit_));
    if let (Some(pil), Some(mat)) = (pil, mat) {
        let a = compose(matrix, &pil.world);
        let b = compose(matrix, &mat.world);
        let d = -((a[3] - b[3]) * heading.cos() + (a[7] - b[7]) * heading.sin());
        out["pillow"] = json!(if d > 0.05 { 1 } else if d < -0.05 { -1 } else { 0 });
    }
    Some(out)
}

/// Python `round(x, n)`: the correctly rounded decimal of the binary value (ties to even on the exact expansion), as a float again.
/// Scaling (`x * 10^n`) is exact enough away from a tie; within 1e-6 of one the decimal expansion decides (EMPIRICAL: a door's reach of
/// 1.10725 + 1 ulp rounds to 1.1073 in Python and to 1.1072 by scaling).
pub fn round_n(v: f64, n: i32) -> f64 {
    let f = 10f64.powi(n);
    let x = v * f;
    if !x.is_finite() || n < 0 {
        return x.round() / f;
    }
    if ((x - x.trunc()).abs() - 0.5).abs() < 1e-6 {
        return format!("{:.*}", n as usize, v).parse().unwrap_or(v);
    }
    x.round() / f
}

fn rv(v: &Value, nd: i32) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => json!(round_n(n.as_f64().unwrap(), nd)),
        Value::Array(a) => Value::Array(a.iter().map(|x| rv(x, nd)).collect()),
        o => o.clone(),
    }
}

/// A lie_feed entry with its floats rounded for the JSON feed (3 decimals, 4 for the angles).
pub fn rounded(lie: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in lie.as_object().unwrap() {
        let nv = match k.as_str() {
            "yaw" | "pitch" | "heading" => rv(v, 4),
            "lim" => v.clone(),
            _ => rv(v, 3),
        };
        out.insert(k.clone(), nv);
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------------------------- clip table
use crate::character::Character;
use p4k_formats::skeleton::{self, Q, V};

/// Model-space pose `(q, p)` per bone of a clip at a frame: the stored parent-relative tracks over the rest pose.
fn clip_pose(ch: &Character, key: &str, frame: f64) -> Option<Vec<(Q, V)>> {
    let bones = ch.bones().ok()?;
    let clip = ch.clip(key)?;
    let full = ch.crc_index().ok()?;
    let mut loc: Vec<(Q, V)> = bones.iter().map(|b| (b.rq, b.rp)).collect();
    for (crc, t) in clip.tracks().iter() {
        if let Some(&i) = full.get(&crc) {
            let (q, p) = t.sample(frame);
            loc[i] = (q.unwrap_or(bones[i].rq), p.unwrap_or(bones[i].rp));
        }
    }
    Some(skeleton::world_poses(bones, &loc))
}

/// (position, heading) of the World bone of a clip at a frame.
fn clip_root(ch: &Character, key: &str, frame: f64) -> Option<(V3, f64)> {
    let w = clip_pose(ch, key, frame)?;
    let i = ch.bones().ok()?.iter().position(|b| b.name == "World")?;
    let (q, p) = w[i];
    let d = skeleton::qrot(q, [0.0, 1.0, 0.0]);
    Some((p, d[1].atan2(d[0])))
}

/// Bed heights (cm) that have their own enter / exit clips and the stem patterns of each (EMPIRICAL).
const HEIGHTS: [(i64, &[&str]); 4] = [
    (30, &["use_lie030_ntrl_{w}_{s}2_01"]),
    (50, &["use_lie050_ntrl_{w}_{s}2_01", "use_lie_bed50_{w}_{s}2_ntrl_01"]),
    (128, &["use_lie128_ntrl_{w}_{s}2_01"]),
    (188, &["use_lie188_ntrl_{w}_{s}2_01"]),
];

/// The enter / exit clips of lying down by bed height and side: `{idle, enter: {height: {side: {id, secs, rel}}}, exit}`.
pub fn clip_table(ch: &Character) -> Value {
    let mut enter = serde_json::Map::new();
    let mut exit = serde_json::Map::new();
    for (h, pats) in HEIGHTS {
        for what in ["enter", "exit"] {
            for s in ["l", "r", "b"] {
                for pat in pats {
                    let stem = pat.replace("{w}", what).replace("{s}", s);
                    let Some(key) = ch.clip_by_stem(&stem) else { continue };
                    let Some(cl) = ch.clip(&key) else { continue };
                    let (Some((p0, h0)), Some((p1, h1))) = (clip_root(ch, &key, 0.0), clip_root(ch, &key, cl.frames as f64)) else { continue };
                    let (ref_p, ref_h, oth_p, oth_h) = if what == "enter" { (p1, h1, p0, h0) } else { (p0, h0, p1, h1) };
                    let rel = to_local(ref_p, ref_h, oth_p);
                    // the side is read from where the standing end is, not from the clip name (EMPIRICAL: the 030 / 128 'l2' clips start on the right)
                    let side = if rel[1] > 1.0 && rel[0].abs() < 0.5 { "b" } else if rel[0] < 0.0 { "l" } else { "r" };
                    let table = if what == "enter" { &mut enter } else { &mut exit };
                    let by_side = table.entry(h.to_string()).or_insert_with(|| json!({})).as_object_mut().unwrap();
                    by_side.entry(side.to_string()).or_insert_with(|| {
                        json!({"id": key, "secs": round_n(cl.seconds, 4), "rel": [round_n(rel[0], 3), round_n(rel[1], 3), round_n(rel[2], 3), round_n(oth_h - ref_h, 4)]})
                    });
                    break;
                }
            }
        }
    }
    json!({"idle": ch.clip_by_stem("use_lie_00_ntrl_a_idle_01"), "enter": enter, "exit": exit})
}

#[cfg(test)]
mod round_tests {
    use super::round_n;

    #[test]
    fn python_round_on_ties() {
        // CPython 3.14: round(1.10725, 4) == 1.1073, round(0.12345, 4) == 0.1235, round(2.675, 2) == 2.67 (binary value below the tie)
        assert_eq!(round_n(1.10725, 4), 1.1073);
        assert_eq!(round_n(0.12345, 4), 0.1235);
        assert_eq!(round_n(2.675, 2), 2.67);
        assert!(round_n(-0.00004, 4).is_sign_negative());
    }
}
