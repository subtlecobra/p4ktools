//! Skeletons of '#ivo' characters (`.chr` and `.skin`) and the vertex skin data of `.skinm` meshes, plus the quaternion helpers the pose
//! maths needs.
//!
//! Nothing here is confirmed against the engine: every layout is EMPIRICAL, derived from `bhm_skeleton_v7.chr`,
//! `male_v7/body/m_body.skin` and `m_body.skinm` (Data.p4k, 4.x).
//!
//! Bone chunk 0xc2011111 (version 0x900) of a `.chr` / `.skin` (EMPIRICAL): u32 count n, then n x 68-byte records, then n NUL separated
//! bone names (same order) up to the end of the chunk.  Record: +0 u32 CRC32 of the exact, case-sensitive name (220 of 220 on the male
//! skeleton; the joint id the .dba / .caf tracks use), +4 u32 (second hash, unknown meaning), +8 i32 parent index (-1 for 'World'; parents
//! come before children), +12 quaternion xyzw float32 + +28 position xyz = the bone relative to its parent in the bind pose; +40
//! quaternion + +56 position = the same bone in the model space of the bind pose.  Units are metres, +Y forward, +Z up.
//!
//! `.skinm` skin stream 0x6eca3708 (stride 24, one entry per vertex; EMPIRICAL, m_body.skinm 12,356 vertices): 8 x u16 bone indices,
//! 8 x u8 weights adding up to exactly 255.  The indices refer to the `.skin`'s sub-skeleton, not the full one.

use crate::error::*;
use crate::ivo::{self, IvoFile, Mesh};
use crate::xmlq::{crc32, latin1, split_nul};

pub const CH_BONES: u32 = 0xC2011111;
pub const STREAM_SKIN: u32 = 0x6ECA3708;
/// The other skin stream, stride 12: 4 x u16 bone index (into the `.skin`'s own bone list), then 4 x u8 weight.  EMPIRICAL: on 427 distinct models
/// (5,005,271 vertices) every weight set sums to 255 and no index passes the bone list.  2,204 of 4,734 skinned item classes use it, armor and
/// clothing as well as small items; every skinned model has one of the two streams.
pub const STREAM_SKIN4: u32 = 0x677C7B23;
const RECORD: usize = 68;

pub type Q = [f64; 4];
pub type V = [f64; 3];

#[derive(Debug, Clone, PartialEq)]
pub struct Bone {
    pub name: String,
    pub crc: u32,
    pub parent: i32,
    pub rq: Q,
    pub rp: V,
    pub aq: Q,
    pub ap: V,
}

/// Bones of a `.chr` / `.skin` file.  Error without a bone chunk.
pub fn parse_bones(data: &[u8]) -> Result<Vec<Bone>> {
    let f = IvoFile::new(data)?;
    let ch = f.get(CH_BONES).ok_or_else(|| Error::msg("no bone chunk"))?;
    let n = u32_at(data, ch.offset)? as usize;
    let end = ch.offset + ch.size;
    let names = split_nul(ivo::slice(data, ch.offset + 4 + RECORD * n, end), usize::MAX - 1);
    let mut out = Vec::with_capacity(n.min(1 << 16));
    for i in 0..n {
        let o = ch.offset + 4 + RECORD * i;
        let crc = u32_at(data, o)?;
        let par = u32_at(data, o + 8)? as i32;
        let name = names.get(i).map(|b| latin1(b)).unwrap_or_default();
        out.push(Bone { name, crc, parent: par, rq: f32s::<4>(data, o + 12)?, rp: f32s::<3>(data, o + 28)?, aq: f32s::<4>(data, o + 40)?, ap: f32s::<3>(data, o + 56)? });
    }
    Ok(out)
}

/// Joint id of a bone name (CRC32 of the exact name, latin-1 bytes).
pub fn crc_of(name: &str) -> u32 {
    let b: Vec<u8> = name.chars().map(|c| c as u32 as u8).collect();
    crc32(&b)
}

/// (8 bone indices per vertex, 8 weights 0..255 per vertex) of a parsed mesh, or empty vectors without a skin stream.  A `STREAM_SKIN4` mesh fills the
/// first four slots.
pub fn mesh_skin(m: &Mesh) -> Result<(Vec<[u16; 8]>, Vec<[u8; 8]>)> {
    let Some(s) = m.streams.get(&STREAM_SKIN) else { return mesh_skin4(m) };
    let (mut idx, mut wt) = (Vec::with_capacity(s.cnt), Vec::with_capacity(s.cnt));
    for i in 0..s.cnt {
        let o = s.off + s.stride * i;
        let mut a = [0u16; 8];
        let mut w = [0u8; 8];
        for k in 0..8 {
            a[k] = u16_at(m.buf, o + 2 * k)?;
            w[k] = u8_at(m.buf, o + 16 + k)?;
        }
        idx.push(a);
        wt.push(w);
    }
    Ok((idx, wt))
}

fn mesh_skin4(m: &Mesh) -> Result<(Vec<[u16; 8]>, Vec<[u8; 8]>)> {
    let Some(s) = m.streams.get(&STREAM_SKIN4).filter(|s| s.stride == 12) else { return Ok((vec![], vec![])) };
    let (mut idx, mut wt) = (Vec::with_capacity(s.cnt), Vec::with_capacity(s.cnt));
    for i in 0..s.cnt {
        let o = s.off + 12 * i;
        let (mut a, mut w) = ([0u16; 8], [0u8; 8]);
        for k in 0..4 {
            a[k] = u16_at(m.buf, o + 2 * k)?;
            w[k] = u8_at(m.buf, o + 8 + k)?;
        }
        idx.push(a);
        wt.push(w);
    }
    Ok((idx, wt))
}

// ---------------------------------------------------------------------------------------------------- quaternion maths (xyzw)
pub fn qmul(a: Q, b: Q) -> Q {
    let (ax, ay, az, aw) = (a[0], a[1], a[2], a[3]);
    let (bx, by, bz, bw) = (b[0], b[1], b[2], b[3]);
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// Rotate vector v by the unit quaternion q.
pub fn qrot(q: Q, v: V) -> V {
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    let (tx, ty, tz) = (2.0 * (y * v[2] - z * v[1]), 2.0 * (z * v[0] - x * v[2]), 2.0 * (x * v[1] - y * v[0]));
    [v[0] + w * tx + (y * tz - z * ty), v[1] + w * ty + (z * tx - x * tz), v[2] + w * tz + (x * ty - y * tx)]
}

pub fn qconj(q: Q) -> Q {
    [-q[0], -q[1], -q[2], q[3]]
}

pub fn qnorm(q: Q) -> Q {
    // length, or 1.0 for a zero quaternion; the sum adds left to right
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    let n = if n == 0.0 { 1.0 } else { n };
    [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
}

/// Column-major 4x4 (16 floats, for WebGL) of rotation q and translation p.
pub fn q_to_mat(q: Q, p: V) -> [f64; 16] {
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    [
        1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y + z * w), 2.0 * (x * z - y * w), 0.0,
        2.0 * (x * y - z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z + x * w), 0.0,
        2.0 * (x * z + y * w), 2.0 * (y * z - x * w), 1.0 - 2.0 * (x * x + y * y), 0.0,
        p[0], p[1], p[2], 1.0,
    ]
}

/// Model-space (q, p) per bone from the parent-relative (q, p) per bone (parents come first).
pub fn world_poses(bones: &[Bone], local: &[(Q, V)]) -> Vec<(Q, V)> {
    let mut out: Vec<(Q, V)> = Vec::with_capacity(bones.len());
    for (b, &(q, p)) in bones.iter().zip(local) {
        if b.parent < 0 {
            out.push((q, p));
        } else {
            let (pq, pp) = out[b.parent as usize];
            let r = qrot(pq, p);
            out.push((qnorm(qmul(pq, q)), [pp[0] + r[0], pp[1] + r[1], pp[2] + r[2]]));
        }
    }
    out
}

/// Skinning matrices (q, p) = world pose * inverse(bind pose) per bone.
pub fn skin_matrices(world: &[(Q, V)], bind: &[(Q, V)]) -> Vec<(Q, V)> {
    world
        .iter()
        .zip(bind)
        .map(|(&(wq, wp), &(bq, bp))| {
            let iq = qconj(bq);
            let ip = qrot(iq, [-bp[0], -bp[1], -bp[2]]);
            let r = qrot(wq, ip);
            (qnorm(qmul(wq, iq)), [wp[0] + r[0], wp[1] + r[1], wp[2] + r[2]])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_with(buf: &[u8], id: u32, stride: usize, cnt: usize) -> Mesh<'_> {
        Mesh {
            buf,
            flags: 0,
            n_verts: cnt,
            n_indices: 0,
            bbox_min: [0.0; 3],
            bbox_max: [0.0; 3],
            vbox_min: [0.0; 3],
            vbox_max: [0.0; 3],
            vertex_format: 2,
            subsets: vec![],
            streams: [(id, ivo::Stream { stride, off: 0, cnt })].into_iter().collect(),
        }
    }

    #[test]
    fn the_four_bone_stream_fills_the_first_four_slots() {
        let buf = [51, 0, 2, 1, 0, 0, 0, 0, 200, 55, 0, 0];
        let (idx, wt) = mesh_skin(&mesh_with(&buf, STREAM_SKIN4, 12, 1)).unwrap();
        assert_eq!(idx, vec![[51, 258, 0, 0, 0, 0, 0, 0]]);
        assert_eq!(wt, vec![[200, 55, 0, 0, 0, 0, 0, 0]]);
    }

    #[test]
    fn a_mesh_without_a_skin_stream_has_no_skin() {
        let (idx, wt) = mesh_skin(&mesh_with(&[0; 12], 0x1234, 12, 1)).unwrap();
        assert!(idx.is_empty() && wt.is_empty());
    }

    #[test]
    fn crc_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc_of("123456789"), 0xCBF43926);
    }

    #[test]
    fn quaternion_identity() {
        let q = [0.0, 0.0, 0.0, 1.0];
        assert_eq!(qrot(q, [1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
        assert_eq!(qnorm([0.0; 4]), [0.0; 4]);
        let m = q_to_mat(q, [1.0, 2.0, 3.0]);
        assert_eq!(&m[12..], &[1.0, 2.0, 3.0, 1.0]);
    }
}
