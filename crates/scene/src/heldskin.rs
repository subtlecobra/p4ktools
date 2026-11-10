//! A hand-held item with its own skeleton and mannequin animations (the grenade's pin / safety-clip removal): the skinned mesh, the bone
//! hierarchy with bind pose and the sampled clips, for the weapons lab.
//!
//! * CONFIRMED on the archive: `gren_frag_behr_mk4.cdf` names the model `gren_frag_behr_mk4.chr` (own skeleton) and one
//!   CA_SKIN attachment `gren_frag_behr_MK4_parts.skin`; its `.chrparams` points at `Animations/fps_weapons/grenades/behr/mk4` (`*/*.caf`).  The
//!   clips are stand-alone `.caf` files `throwable_open_01.caf` / `throwable_close_01.caf` (KSAR: `ksar_gren_frag_throwable_*_01.caf` in
//!   `.../grenades/ksar/frag`).  The ADB `Mannequin/ADB/Items/Weapons/FPS/Grenades/ksar_gren_frag_01.adb` maps the fragment `primeState` tag `primed`
//!   to `*_throwable_open_01` and `unprimed` to `*_throwable_close_01`: open = the pin / clip comes out, close = it goes back.
//! * Skinning is the character's (`character.rs`): the mesh's bone indices refer to the `.skin`'s own bone list, mapped to the `.chr` skeleton by CRC32
//!   of the bone name (nearest known ancestor otherwise); clip tracks are keyed by the same CRC32; a bone without a track keeps its rest pose.
//! * EMPIRICAL: the mesh positions are in the bind pose of the `.chr` (the static grenade the lab draws is that same mesh, `weaponmodel::skin_parts`).

use base64::Engine;
use p4k_formats::cdf;
use p4k_formats::dba;
use p4k_formats::ivo;
use p4k_formats::skeleton::{self, Bone};
use p4k::P4k;
use serde_json::{json, Map, Value};

use crate::weaponmodel::skin_parts;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// The two clips of a grenade class: (open, close) as archive paths, or None for an unknown class.  CONFIRMED by `find` on the archive.
pub fn clip_paths(cls: &str) -> Option<(&'static str, &'static str)> {
    let c = cls.to_lowercase();
    if c.starts_with("behr_gren_frag") {
        Some(("Data/Animations/fps_weapons/grenades/behr/mk4/throwable_open_01.caf", "Data/Animations/fps_weapons/grenades/behr/mk4/throwable_close_01.caf"))
    } else if c.starts_with("ksar_gren_frag") {
        Some((
            "Data/Animations/fps_weapons/grenades/ksar/frag/ksar_gren_frag_throwable_open_01.caf",
            "Data/Animations/fps_weapons/grenades/ksar/frag/ksar_gren_frag_throwable_close_01.caf",
        ))
    } else {
        None
    }
}

/// The four strongest influences of a vertex (indices mapped through `map`, weights rescaled to sum to exactly 255).
pub fn top4(idx: &[u16; 8], w: &[u8; 8], map: &[usize]) -> ([u8; 4], [u8; 4]) {
    let mut pairs: Vec<(u32, usize)> = idx.iter().zip(w.iter()).filter(|(_, w)| **w != 0).map(|(i, w)| (*w as u32, map.get(*i as usize).copied().unwrap_or(0))).collect();
    pairs.sort_by(|a, b| b.cmp(a));
    pairs.truncate(4);
    let tot: u32 = pairs.iter().map(|p| p.0).sum();
    if tot == 0 {
        return ([0; 4], [255, 0, 0, 0]);
    }
    let (mut b, mut ws) = ([0u8; 4], [0i32; 4]);
    for (k, (wt, bi)) in pairs.iter().enumerate() {
        b[k] = (*bi).min(255) as u8;
        ws[k] = (*wt as f64 * 255.0 / tot as f64).round() as i32;
    }
    ws[0] += 255 - ws.iter().sum::<i32>();
    (b, [ws[0] as u8, ws[1] as u8, ws[2] as u8, ws[3] as u8])
}

/// Parent-relative (q xyzw, p) of every bone at `frame`: the clip's track where it has one, else the rest pose.
pub fn local_pose(bones: &[Bone], clip: &dba::Clip, frame: f64) -> Vec<[f32; 7]> {
    let tm = clip.tracks();
    bones
        .iter()
        .map(|b| {
            let (q, p) = match tm.get(b.crc) {
                Some(t) => t.sample(frame),
                None => (None, None),
            };
            let q = q.unwrap_or(b.rq);
            let p = p.unwrap_or(b.rp);
            [q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32, p[0] as f32, p[1] as f32, p[2] as f32]
        })
        .collect()
}

fn clip_json(p4k: &P4k, bones: &[Bone], path: &str) -> Result<Value, String> {
    let ent = p4k.find_path(path, ".caf").ok_or_else(|| format!("clip not in the archive: {}", path))?;
    let data = p4k.read(ent).map_err(|e| e.to_string())?;
    let clip = dba::parse_caf(&data, path).map_err(|e| e.to_string())?;
    let n = clip.frames.max(1) as usize + 1;
    let known = clip.bones.iter().filter(|c| bones.iter().any(|b| b.crc == **c)).count();
    let mut raw = Vec::with_capacity(n * bones.len() * 28);
    for f in 0..n {
        for v in local_pose(bones, &clip, f as f64) {
            for c in v {
                raw.extend(c.to_le_bytes());
            }
        }
    }
    Ok(json!({"n": n, "fps": 30, "secs": clip.seconds, "tracks": clip.bones.len(), "matched": known, "d": b64(&raw)}))
}

/// `{bones, parts, clips}` for the grenade class whose model is `cdf_path` (see the module docs).
pub fn held_skin(p4k: &P4k, cls: &str, cdf_path: &str) -> Result<Value, String> {
    let (open, close) = clip_paths(cls).ok_or_else(|| format!("no animated model known for {}", cls))?;
    let ent = p4k.find_path(cdf_path, ".cdf").ok_or("cdf not in the archive")?;
    let c = cdf::parse_cdf(&p4k.read(ent).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let chr = p4k.find_path(&c.model, ".chr").ok_or("chr not in the archive")?;
    let bones = skeleton::parse_bones(&p4k.read(chr).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let att = c.attachments.iter().find(|a| a.atype == "CA_SKIN" && !a.binding.is_empty()).ok_or("no CA_SKIN attachment")?;
    let skin_path = att.binding.replace('\\', "/");
    let skin_ent = p4k.find_path(&skin_path, ".skin").ok_or("skin not in the archive")?;
    let skin = p4k.read(skin_ent).map_err(|e| e.to_string())?;
    let part_bones = skeleton::parse_bones(&skin).map_err(|e| e.to_string())?;
    let stem = &skin_path[..skin_path.rfind('.').unwrap_or(skin_path.len())];
    let skinm_ent = p4k.find_path(&format!("{}.skinm", stem), ".skinm").ok_or("skinm not in the archive")?;
    let skinm = p4k.read(skinm_ent).map_err(|e| e.to_string())?;
    let meshes = ivo::parse_cgfm(&skinm).map_err(|e| e.to_string())?;
    let mesh = meshes.first().ok_or("no mesh")?;
    let (idx, wts) = skeleton::mesh_skin(mesh).map_err(|e| e.to_string())?;
    if idx.is_empty() {
        return Err("the mesh has no skin stream".into());
    }
    let full: std::collections::HashMap<u32, usize> = bones.iter().enumerate().map(|(i, b)| (b.crc, i)).collect();
    // a .skin bone absent from the .chr follows its nearest known ancestor
    let map: Vec<usize> = (0..part_bones.len())
        .map(|i| {
            let mut j = i as i64;
            while j >= 0 && !full.contains_key(&part_bones[j as usize].crc) {
                j = part_bones[j as usize].parent as i64;
            }
            if j >= 0 {
                full[&part_bones[j as usize].crc]
            } else {
                0
            }
        })
        .collect();
    let mut parts = vec![];
    for p in skin_parts(p4k, &att.binding, "") {
        let (mut b4, mut w4) = (Vec::with_capacity(p.src.len() * 4), Vec::with_capacity(p.src.len() * 4));
        for &s in &p.src {
            let (b, w) = match (idx.get(s as usize), wts.get(s as usize)) {
                (Some(i), Some(w)) => top4(i, w, &map),
                _ => ([0; 4], [255, 0, 0, 0]),
            };
            b4.extend(b);
            w4.extend(w);
        }
        let f32s = |it: &mut dyn Iterator<Item = f64>| -> Vec<u8> { it.flat_map(|x| (x as f32).to_le_bytes()).collect() };
        let mut o = Map::new();
        o.insert("nv".into(), json!(p.verts.len()));
        o.insert("nt".into(), json!(p.tris.len()));
        o.insert("sub".into(), json!(p.key.as_ref().map_or(-1, |k| k.1)));
        o.insert("v".into(), json!(b64(&f32s(&mut p.verts.iter().flatten().copied()))));
        o.insert("uv".into(), json!(b64(&f32s(&mut p.uv.iter().flatten().copied()))));
        if p.nrm.len() == p.verts.len() {
            o.insert("n".into(), json!(b64(&f32s(&mut p.nrm.iter().flatten().copied()))));
        }
        o.insert("i".into(), json!(b64(&p.tris.iter().flatten().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>())));
        o.insert("b".into(), json!(b64(&b4)));
        o.insert("w".into(), json!(b64(&w4)));
        parts.push(Value::Object(o));
    }
    let pack = |f: &dyn Fn(&Bone) -> [f64; 7]| -> Vec<f64> { bones.iter().flat_map(|b| f(b)).collect() };
    let rest = pack(&|b| [b.rq[0], b.rq[1], b.rq[2], b.rq[3], b.rp[0], b.rp[1], b.rp[2]]);
    let bind = pack(&|b| [b.aq[0], b.aq[1], b.aq[2], b.aq[3], b.ap[0], b.ap[1], b.ap[2]]);
    Ok(json!({
        "bones": {"names": bones.iter().map(|b| b.name.clone()).collect::<Vec<_>>(), "parent": bones.iter().map(|b| b.parent).collect::<Vec<_>>(), "rest": rest, "bind": bind},
        "parts": parts,
        "clips": {"open": clip_json(p4k, &bones, open)?, "close": clip_json(p4k, &bones, close)?},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top4_keeps_the_strongest_and_sums_to_255() {
        let idx = [0u16, 1, 2, 3, 4, 5, 0, 0];
        let w = [10u8, 100, 50, 20, 5, 70, 0, 0];
        let (b, ws) = top4(&idx, &w, &[10, 11, 12, 13, 14, 15]);
        assert_eq!(b, [11, 15, 12, 13]);
        assert_eq!(ws.iter().map(|x| *x as u32).sum::<u32>(), 255);
        assert!(ws[0] >= ws[1] && ws[1] >= ws[2]);
        assert_eq!(top4(&[0; 8], &[0; 8], &[3]), ([0; 4], [255, 0, 0, 0]));
    }

    #[test]
    fn clip_paths_by_class() {
        assert!(clip_paths("behr_gren_frag_01").unwrap().0.ends_with("/mk4/throwable_open_01.caf"));
        assert!(clip_paths("ksar_gren_frag_01").unwrap().1.ends_with("throwable_close_01.caf"));
        assert!(clip_paths("rifle").is_none());
    }

    /// Real archive: the MK4 grenade skins to its own skeleton and both clips find their bones.  Skips without the archive.
    #[test]
    fn real_archive_mk4() {
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = P4k::open(std::path::Path::new(&path)) else { return };
        let v = held_skin(&p4k, "behr_gren_frag_01", "Data/Objects/fps_weapons/grenades/behr/mk4/gren_frag_behr_mk4.cdf").expect("skin");
        let nb = v["bones"]["names"].as_array().unwrap().len();
        assert!(nb > 1);
        for k in ["open", "close"] {
            let c = &v["clips"][k];
            assert!(c["n"].as_u64().unwrap() > 2, "{}", k);
            assert_eq!(c["matched"], c["tracks"], "every {} track finds its bone", k);
        }
        eprintln!("{}", json!({"bones": v["bones"]["names"], "open": {"n": v["clips"]["open"]["n"], "tracks": v["clips"]["open"]["tracks"]}, "close": v["clips"]["close"]["n"]}));
    }
}
