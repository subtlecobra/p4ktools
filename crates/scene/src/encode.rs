//! Scene -> JSON value (typed arrays as base64) consumed by the web viewer.

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::builder::{Info, Part};
use crate::materials::MatRegistry;
use p4k_formats::error::py_round;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Unit-vector components (-1..1) as int8 snorm (v * 127, rounded half to even like Python's round, clamped to +-127).
pub fn snorm8(vals: &[f64], out: &mut Vec<u8>) {
    for &c in vals {
        out.push(py_round(c * 127.0).clamp(-127, 127) as i8 as u8);
    }
}

fn f32_bytes(vals: impl Iterator<Item = f64>) -> Vec<u8> {
    let mut v = Vec::new();
    for x in vals {
        v.extend((x as f32).to_le_bytes());
    }
    v
}

/// Parts -> (encoded parts, bbox).  Part keys: name, kind, node, nv, nt, v, i and optionally uv, mat, and the stored tangent frame:
/// n = int8 snorm x3 per vertex (normal), tg = int8 snorm x4 per vertex (tangent xyz, then +-127 = handedness: bitangent = w *
/// cross(N, T); all zero when the mesh has no tangent stream, ivo::rotate_frame).  Both are absent when the source mesh carried
/// no frame stream.  Cost: 7 bytes per vertex before base64 (positions 12, uv 8).  c (only with `colours`) = the vertex colours (4
/// bytes r,g,b,a per vertex) when the part has any that is not white (the item pages read the LayerBlend %DECALS decal UV from them).
pub fn encode_parts(parts: &[Part], colours: bool) -> (Vec<Value>, Value) {
    let mut out = Vec::with_capacity(parts.len());
    let (mut mn, mut mx) = ([1e30f64; 3], [-1e30f64; 3]);
    for pt in parts {
        for v in &pt.verts {
            for k in 0..3 {
                mn[k] = mn[k].min(v[k]);
                mx[k] = mx[k].max(v[k]);
            }
        }
        let va = f32_bytes(pt.verts.iter().flatten().copied());
        let mut ia = Vec::with_capacity(pt.tris.len() * 12);
        for t in &pt.tris {
            for &i in t {
                ia.extend(i.to_le_bytes());
            }
        }
        let mut enc = Map::new();
        enc.insert("name".into(), json!(pt.name));
        enc.insert("kind".into(), json!(pt.kind));
        enc.insert("node".into(), json!(pt.node));
        enc.insert("nv".into(), json!(pt.verts.len()));
        enc.insert("nt".into(), json!(pt.tris.len()));
        enc.insert("v".into(), json!(b64(&va)));
        enc.insert("i".into(), json!(b64(&ia)));
        if !pt.uv.is_empty() {
            enc.insert("uv".into(), json!(b64(&f32_bytes(pt.uv.iter().flatten().copied()))));
        }
        if !pt.nrm.is_empty() && pt.nrm.len() == pt.verts.len() {
            let mut na = Vec::new();
            for n in &pt.nrm {
                snorm8(n, &mut na);
            }
            enc.insert("n".into(), json!(b64(&na)));
            if pt.tan.iter().any(|t| t[3] != 0.0) {
                let mut ta = Vec::new();
                for t in &pt.tan {
                    snorm8(&t[..3], &mut ta);
                    ta.push((t[3] as i64 * 127) as i8 as u8);
                }
                enc.insert("tg".into(), json!(b64(&ta)));
            }
        }
        if colours && !pt.vc.is_empty() && pt.vc.len() == pt.verts.len() && pt.vc.iter().any(|c| *c != [0xFF; 4]) {
            // vertex colour r,g,b,a bytes (ivo::bgra_to_rgba): LayerBlend %DECALS decal UV
            enc.insert("c".into(), json!(b64(&pt.vc.concat())));
        }
        if let Some(m) = pt.mat {
            if m >= 0 {
                enc.insert("mat".into(), json!(m));
            }
        }
        out.push(Value::Object(enc));
    }
    let bbox = if mn[0] > mx[0] { json!([[-1, -1, -1], [1, 1, 1]]) } else { json!([mn, mx]) };
    (out, bbox)
}

/// Single-asset scene (`info`, `bbox` and `parts`).  `registry_notes` are the registry's notes (sorted),
/// appended to info.notes when a registry is given.  `encode_scene_reg` adds the registry's `materials` / `textures`.
pub fn encode_scene(info: &Info, parts: &[Part], registry_notes: &[String]) -> Value {
    let (enc, bbox) = encode_parts(parts, false);
    let mut notes = info.notes.clone();
    notes.extend(registry_notes.iter().cloned());
    let mut i = Map::new();
    i.insert("asset".into(), json!(info.asset));
    i.insert("companion".into(), json!(info.companion));
    i.insert("nodes".into(), json!(info.nodes));
    i.insert("material".into(), json!(info.material));
    i.insert("notes".into(), json!(notes));
    if let Some(v) = info.render_tris {
        i.insert("render_tris".into(), json!(v));
    }
    if let Some(v) = info.render_verts {
        i.insert("render_verts".into(), json!(v));
    }
    if let Some(v) = info.render_subsets {
        i.insert("render_subsets".into(), json!(v));
    }
    i.insert("collision_counts".into(), json!(info.collision_counts));
    i.insert("errors".into(), json!(info.errors));
    json!({"info": Value::Object(i), "bbox": bbox, "parts": enc})
}

/// `encode_scene` plus the registry's `materials` / `textures`: the registry's notes are
/// appended to `info.notes`, the two arrays follow `parts`.
pub fn encode_scene_reg(info: &Info, parts: &[Part], reg: &MatRegistry) -> Value {
    let notes: Vec<String> = reg.notes.iter().cloned().collect();
    let mut v = encode_scene(info, parts, &notes);
    let (m, t) = reg.export();
    v["materials"] = m;
    v["textures"] = t;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snorm_rounds_half_to_even_and_clamps() {
        let mut o = vec![];
        snorm8(&[0.5 / 127.0, 1.5 / 127.0, 2.0, -2.0, -1.0], &mut o);
        assert_eq!(o, vec![0u8, 2, 127, (-127i8) as u8, (-127i8) as u8]);
    }

    #[test]
    fn minimal_part_layout() {
        let p = Part { name: "a".into(), kind: "render".into(), node: 3, verts: vec![[1.0, 2.0, 3.0]], tris: vec![[0, 0, 0]], ..Default::default() };
        let (enc, bbox) = encode_parts(&[p], false);
        assert_eq!(enc[0]["nv"], 1);
        assert_eq!(enc[0]["v"], b64(&[0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64]));
        assert_eq!(bbox, json!([[1.0, 2.0, 3.0], [1.0, 2.0, 3.0]]));
        assert!(enc[0].get("uv").is_none() && enc[0].get("mat").is_none());
        assert_eq!(encode_parts(&[], false).1, json!([[-1, -1, -1], [1, 1, 1]]));
    }
}
