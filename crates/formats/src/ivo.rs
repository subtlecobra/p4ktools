//! Reader for the CIG '#ivo' (chunk version 0x900) geometry containers.
//!
//! Everything below was cross-checked against the engine's loader.  Items that are only verified empirically (against real assets)
//! are marked UNCONFIRMED / EMPIRICAL.
//!
//! Container (file types: 0 = ?, 1 = ?, 2 = .skinm, 3 = .cgf/.cga, 4 = .cgfm mesh file):
//!   +0  u32 magic      ('#ivo' = 0x6f766923)
//!   +4  u32 version    (0x900)
//!   +8  u32 chunkCount (cgf: <= 10, cgfm/skinm: <= 4, checked by the loaders)
//!   +12 u32 tableOffset
//!   table @tableOffset: chunkCount x { u32 type, u32 version(=0x900), u64 offset }
//!   the size of a chunk is not stored: next chunk offset - this offset.
//!
//! .cgf chunk dispatch:
//!   0xbe5e493e header          0x83353333 material names    0x9351756f ?
//!   0x58de1772 physics groups  0x70697fda node table        0x92914444 node/lod info
//!   0x2b7ecf9f, 0x1e14a062, 0xc1c36afe, 0xc201973c (v0x901): bone/helper/extra tables (not parsed here)
//! .cgfm chunk dispatch:
//!   0xb8757777 one mesh   0xb32459d2 mesh set (u32 count + sizes)   0xe0181074 bone map / stream info

use std::collections::HashMap;

use crate::error::*;

pub const IVO_MAGIC: u32 = 0x6F766923;

pub const CH_CGF_HEADER: u32 = 0xBE5E493E;
pub const CH_CGF_MATERIALS: u32 = 0x83353333;
pub const CH_CGF_PHYSICS: u32 = 0x58DE1772;
pub const CH_CGF_NODES: u32 = 0x70697FDA;
pub const CH_CGF_NODEINFO: u32 = 0x92914444;
pub const CH_MESH: u32 = 0xB8757777;
pub const CH_MESHSET: u32 = 0xB32459D2;
pub const CH_MESH_EXTRA: u32 = 0xE0181074;

// ---- stream ids (the engine's table has 14 entries {u32 id, u16 stride, u16 slot, u64 dest}) ----
pub const STREAM_INDEX: u32 = 0xEECDC168; // u16 indices, first stream of every mesh
pub const STREAM_P3F_C4B_T2H: u32 = 0x91329AE9; // stride 20, slot 21, mesh+0x08 (loader error text: eVF_P3F_C4B_T2H)
pub const STREAM_P3S_C4B_T2H: u32 = 0xB3A70D5E; // stride 16, slot 23, mesh+0x50 (loader error text: eVF_P3S_C4B_T2H)

/// Slice `data[a..b]` clamped to the buffer.
pub fn slice(data: &[u8], a: usize, b: usize) -> &[u8] {
    let b = b.min(data.len());
    let a = a.min(b);
    &data[a..b]
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub ctype: u32,
    pub version: u32,
    pub offset: usize,
    pub size: usize,
}

pub struct IvoFile<'a> {
    pub data: &'a [u8],
    pub version: u32,
    pub chunks: Vec<Chunk>,
}

impl<'a> IvoFile<'a> {
    /// Raises `Error::NotIvo` when the file is not #ivo geometry.  EMPIRICAL (hangar_medfrnt_dungeon_exec_rund.socpak: all 17
    /// designer brushes, e.g. brush/designer_638.cgf, 3,052 bytes): they start with the ASCII magic 'CrCh' (older CryEngine chunk
    /// container) and are not parsed here; no engine evidence for their layout has been collected, so callers treat them as
    /// geometry that cannot be read.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        let magic = u32_at(data, 0)?;
        let ver = u32_at(data, 4)?;
        let n = u32_at(data, 8)? as usize;
        let tab = u32_at(data, 12)? as usize;
        if magic != IVO_MAGIC {
            return Err(Error::NotIvo(if data.starts_with(b"CrCh") { "not an #ivo file (legacy CrCh chunk file)".into() } else { "not an #ivo file".into() }));
        }
        let mut raw = Vec::with_capacity(n);
        for i in 0..n {
            let o = tab + 16 * i;
            raw.push((u32_at(data, o)?, u32_at(data, o + 4)?, u64_at(data, o + 8)? as usize));
        }
        let maxo = raw.iter().map(|r| r.2).max().ok_or_else(|| Error::msg("max() arg is an empty sequence"))?;
        let mut offs: Vec<usize> = raw.iter().map(|r| r.2).collect();
        offs.sort_unstable();
        offs.push(if tab > maxo { tab } else { data.len() });
        let chunks = raw
            .iter()
            .map(|&(t, v, o)| {
                let nxt = offs.iter().copied().filter(|&x| x > o).min().unwrap_or(data.len());
                Chunk { ctype: t, version: v, offset: o, size: nxt.saturating_sub(o) }
            })
            .collect();
        Ok(IvoFile { data, version: ver, chunks })
    }

    pub fn get(&self, ctype: u32) -> Option<&Chunk> {
        self.chunks.iter().find(|c| c.ctype == ctype)
    }

    pub fn bytes(&self, ch: &Chunk) -> &'a [u8] {
        slice(self.data, ch.offset, ch.offset + ch.size)
    }
}

// ---------------------------------------------------------------------------------------------
// Mesh chunk 0xb8757777
// ---------------------------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct Subset {
    pub mat_id: u32,
    pub first_index: u32,
    pub num_indices: u32,
    pub first_vertex: u32,
    pub num_vertices: u32,
    pub radius: f64,
    pub center: [f64; 3],
    pub texel_density: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Stream {
    pub stride: usize,
    pub off: usize,
    pub cnt: usize,
}

pub struct Mesh<'a> {
    pub buf: &'a [u8],
    pub flags: u32,
    pub n_verts: usize,
    pub n_indices: usize,
    pub bbox_min: [f64; 3],
    pub bbox_max: [f64; 3],
    /// Second box (+0x30/+0x3c, mesh+0x154): bounds of the stored (node-local) vertex space; equals the first for single-object
    /// meshes and is the quantisation box of the P3S positions.
    pub vbox_min: [f64; 3],
    pub vbox_max: [f64; 3],
    /// 2 = P3F_C4B_T2H, 4 = P3S_C4B_T2H (checked by the loader)
    pub vertex_format: u8,
    pub subsets: Vec<Subset>,
    pub streams: HashMap<u32, Stream>,
}

/// `buf[base..]` is one mesh chunk.  The 8-byte stream alignment the loader does on absolute addresses is done on buffer offsets
/// (the file buffer itself is 8-aligned; `abs_base` is 0 for every caller).
pub fn parse_mesh(buf: &[u8], base: usize) -> Result<Mesh<'_>> {
    let flags = u32_at(buf, base)?;
    let n_streams = u32_at(buf, base + 4)? as usize;
    let n_verts = u32_at(buf, base + 8)? as usize;
    let n_indices = u32_at(buf, base + 12)? as usize;
    let n_subsets = u32_at(buf, base + 16)? as usize;
    let bbox_min = f32s::<3>(buf, base + 0x18)?; // copied to mesh+0x13c (ParseMesh)
    let bbox_max = f32s::<3>(buf, base + 0x24)?;
    let vbox_min = f32s::<3>(buf, base + 0x30)?;
    let vbox_max = f32s::<3>(buf, base + 0x3C)?;
    let vertex_format = u8_at(buf, base + 0x48)?;
    let n_arr2 = u32_at(buf, base + 0x50)? as usize;
    let n_arr3 = u32_at(buf, base + 0x4C)? as usize;
    let mut p = base + 0xA8;
    let mut subsets = Vec::with_capacity(n_subsets.min(1 << 20));
    for i in 0..n_subsets {
        let o = p + 0x30 * i;
        subsets.push(Subset {
            mat_id: u32_at(buf, o)?,
            first_index: u32_at(buf, o + 4)?,
            num_indices: u32_at(buf, o + 8)?,
            first_vertex: u32_at(buf, o + 12)?,
            num_vertices: u32_at(buf, o + 20)?,
            radius: f32_at(buf, o + 24)?,
            center: f32s::<3>(buf, o + 28)?,
            texel_density: f32_at(buf, o + 40)?,
        });
    }
    p += 0x30 * n_subsets + 0x30 * n_arr2 + 4 * n_arr3;
    p += p.wrapping_neg() & 7;
    let mut streams = HashMap::new();
    for si in 0..n_streams {
        let sid = u32_at(buf, p)?;
        let stride = u16_at(buf, p + 4)? as usize;
        let cnt = if si == 0 { n_indices } else { n_verts };
        if si == 0 && sid != STREAM_INDEX {
            return Err(Error::msg("first stream is not the index stream"));
        }
        streams.insert(sid, Stream { stride, off: p + 8, cnt });
        p += 8 + stride * cnt;
        p += p.wrapping_neg() & 7;
    }
    Ok(Mesh { buf, flags, n_verts, n_indices, bbox_min, bbox_max, vbox_min, vbox_max, vertex_format, subsets, streams })
}

impl<'a> Mesh<'a> {
    pub fn indices(&self) -> Result<Vec<u16>> {
        let s = self.streams.get(&STREAM_INDEX).ok_or_else(|| Error::msg("KeyError: index stream"))?;
        let b = self.buf.get(s.off..s.off + 2 * s.cnt).ok_or_else(|| Error::msg("unpack_from requires a larger buffer (indices)"))?;
        Ok(b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }

    /// Vertex positions in the mesh's own space.
    ///
    /// P3F_C4B_T2H (format 2): 3 x float32 at +0.
    /// P3S_C4B_T2H (format 4): 4 x int16 snorm {x,y,z,w=32767} at +0.  pos_k = center_k + raw_k/32767 * max(1, halfExtent_k)
    /// with center/halfExtent from the SECOND chunk box (+0x30/+0x3c).  EMPIRICAL (the shader-side decode is not in the engine's
    /// loader): reproduces that box's extremes on all sampled meshes and the per-node bboxes of the node table.
    pub fn positions(&self) -> Result<Vec<[f64; 3]>> {
        if let Some(s) = self.streams.get(&STREAM_P3F_C4B_T2H) {
            return (0..s.cnt).map(|i| f32s::<3>(self.buf, s.off + s.stride * i)).collect();
        }
        if let Some(s) = self.streams.get(&STREAM_P3S_C4B_T2H) {
            let mut c = [0.0; 3];
            let mut sc = [0.0; 3];
            for k in 0..3 {
                c[k] = (self.vbox_min[k] + self.vbox_max[k]) * 0.5;
                sc[k] = f64::max(1.0, (self.vbox_max[k] - self.vbox_min[k]) * 0.5) / 32767.0;
            }
            let mut out = Vec::with_capacity(s.cnt);
            for i in 0..s.cnt {
                let o = s.off + s.stride * i;
                let (x, y, z) = (i16_at(self.buf, o)? as f64, i16_at(self.buf, o + 2)? as f64, i16_at(self.buf, o + 4)? as f64);
                out.push([c[0] + x * sc[0], c[1] + y * sc[1], c[2] + z * sc[2]]);
            }
            return Ok(out);
        }
        Ok(vec![])
    }

    fn vertex_stream(&self) -> Option<(Stream, usize, usize)> {
        // (stream, uv offset, colour offset)
        if let Some(s) = self.streams.get(&STREAM_P3F_C4B_T2H) {
            Some((*s, 16, 12))
        } else {
            self.streams.get(&STREAM_P3S_C4B_T2H).map(|s| (*s, 12, 8))
        }
    }

    /// Per-vertex texture coordinates (u, v) from the T2H part of the vertex (two float16).
    ///
    /// Offsets: P3F_C4B_T2H (stride 20) -> +16, P3S_C4B_T2H (stride 16) -> +12.  EMPIRICAL: the values at +12 / +16 are sane
    /// tiling coordinates on the sampled assets while +8 decodes to NaN half floats.
    pub fn uvs(&self) -> Result<Vec<[f64; 2]>> {
        let Some((s, uo, _)) = self.vertex_stream() else { return Ok(vec![]) };
        (0..s.cnt)
            .map(|i| {
                let o = s.off + s.stride * i + uo;
                Ok([half(u16_at(self.buf, o)?), half(u16_at(self.buf, o + 2)?)])
            })
            .collect()
    }

    /// Per-vertex colour (4 bytes, in memory order, which is B, G, R, A: see `bgra_to_rgba`) from the C4B part of the vertex.
    ///
    /// Offsets: P3F_C4B_T2H (stride 20) -> +12, P3S_C4B_T2H (stride 16) -> +8 (the 4 bytes between the position and the UVs).
    /// EMPIRICAL: position of the field only; the channel order is not confirmed here.
    pub fn colors(&self) -> Result<Vec<[u8; 4]>> {
        let Some((s, _, co)) = self.vertex_stream() else { return Ok(vec![]) };
        (0..s.cnt)
            .map(|i| {
                let o = s.off + s.stride * i + co;
                self.buf.get(o..o + 4).map(|b| [b[0], b[1], b[2], b[3]]).ok_or_else(|| Error::msg("colour out of range"))
            })
            .collect()
    }
}

/// Vertex offset to add to the indices `seg` of subset `s`.
///
/// EMPIRICAL: index buffers are 16-bit.  A small mesh stores absolute vertex numbers: every index of a subset lies in
/// [first_vertex, first_vertex + num_vertices) (checked on the seat model in the integration test).  A mesh with more than
/// 65,535 vertices cannot: the vertices are split into chunks of at most 65,535 and the indices are relative to the start of
/// the subset's chunk, which is not the subset's own first vertex (Idris_UI_Ship, 592,444 vertices: a subset at vertex
/// 76,205 has indices from 12,742, its chunk starting at 63,463; several subsets share a chunk).  The subset's lowest
/// index is its first vertex, so the chunk offset is first_vertex - lowest index.  61 of 61 sampled meshes over 2 MB have
/// 16-bit indices and more than 65,535 vertices.  Absolute and relative are told apart per subset by the invariant above.
pub fn subset_index_base(s: &Subset, seg: &[u16]) -> i64 {
    if seg.is_empty() {
        return 0;
    }
    let lo = *seg.iter().min().unwrap() as i64;
    let hi = *seg.iter().max().unwrap() as i64;
    let fv = s.first_vertex as i64;
    if fv <= lo && hi < fv + s.num_vertices as i64 {
        return 0;
    }
    fv - lo
}

/// INFERRED (strong, not confirmed in the engine's vertex declaration): the Illum decal pixel shader multiplies the diffuse by
/// COLOR0.b and the real data has decals stored as bytes (255, 0, 0, a) (and glows (255, 255, 255, 255)); only byte 0 = .b gives
/// white decals.  Read as RGBA the same decals came out saturated red.  Reorders one C4B vertex colour from (B, G, R, A) to RGBA.
pub fn bgra_to_rgba(c: [u8; 4]) -> [u8; 4] {
    [c[2], c[1], c[0], c[3]]
}

/// Parse a .cgfm/.cgam/.skinm file into its meshes.
///
/// 0xb8757777: a single mesh.  0xb32459d2: u32 count at +0, u32 A[count] at +0x14, u32 B[count] (mesh
/// byte sizes) after it, then the meshes back to back (each start aligned to 8).
pub fn parse_cgfm(data: &[u8]) -> Result<Vec<Mesh<'_>>> {
    let f = IvoFile::new(data)?;
    let mut meshes = Vec::new();
    for c in &f.chunks {
        if c.ctype == CH_MESH {
            meshes.push(parse_mesh(data, c.offset)?);
        } else if c.ctype == CH_MESHSET {
            let n = u32_at(data, c.offset)? as usize;
            let mut sizes = Vec::with_capacity(n);
            for i in 0..n {
                sizes.push(u32_at(data, c.offset + 0x14 + 4 * n + 4 * i)? as usize);
            }
            let mut p = c.offset + 0x14 + 8 * n;
            p += p.wrapping_neg() & 7;
            for sz in sizes {
                meshes.push(parse_mesh(data, p)?);
                p += sz;
                p += p.wrapping_neg() & 7;
            }
        }
    }
    Ok(meshes)
}

// ---------------------------------------------------------------------------------------------
// Tangent frames
// ---------------------------------------------------------------------------------------------
pub const STREAM_TANGENTS: u32 = 0xEE057252; // stride 8: two u32 per vertex, word0 = tangent, word1 = bitangent (static meshes)
pub const STREAM_NODE_INDEX: u32 = 0x9D51C5EE; // stride 2: u16 owning node per vertex (== subset.mat_id >> 16)
pub const STREAM_QTANGENT: u32 = 0xB95E9A1B; // stride 8: 4 x int16 snorm quaternion (skinned meshes, always present)
pub const STREAM_NORMALS_F3: u32 = 0x7E216CAB; // stride 12: float3 normals (CONFIRMED in the engine; not seen on sampled assets)

const T15: f64 = 16383.0;

pub type V3 = [f64; 3];

/// (normal, tangent, handed): the vertex frame in the mesh's own space.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub n: V3,
    pub t: V3,
    pub h: f64,
}

/// (x, y, z, flip) of one packed tangent / bitangent word of stream 0xee057252.
///
/// bits 0-14 x, bits 15-29 y (each (field - 16383) / 16383), bit 30 = z is negative (z = +-sqrt(max(0, 1 - x^2 - y^2))), bit 31 =
/// handedness flip.  CONFIRMED against the engine's converter that writes this format from two Vec4s (the loader copies the
/// stream raw to the GPU); EMPIRICAL (26,870 vertices of
/// hdgw_rifle_ballistic_01.cgam): x^2+y^2 <= 1 for every word, |T| = |B| = 1, T agrees with dP/du (mean 0.96).
pub fn decode_packed_tangent(w: u32) -> (f64, f64, f64, u32) {
    let x = (((w & 0x7FFF) as i64 - 16383) as f64) / T15;
    let y = ((((w >> 15) & 0x7FFF) as i64 - 16383) as f64) / T15;
    let mut z = f64::max(0.0, 1.0 - x * x - y * y).sqrt();
    if (w >> 30) & 1 != 0 {
        z = -z;
    }
    (x, y, z, w >> 31)
}

pub fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

pub fn unit(v: V3) -> V3 {
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if n > 1e-12 {
        [v[0] / n, v[1] / n, v[2] / n]
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// (normal, tangent, handed) from the two words of a static-mesh vertex.  N = s * normalize(T x B) with s = -1 when word0's bit 31
/// is set (EMPIRICAL: agrees with the geometric normals for 98 % of the vertices of the rifle).  handed = s, so that
/// B = handed * (N x T).
pub fn frame_from_tangent_words(w0: u32, w1: u32) -> Frame {
    let (tx, ty, tz, flip) = decode_packed_tangent(w0);
    let (bx, by, bz, _) = decode_packed_tangent(w1);
    let t = [tx, ty, tz];
    let n = unit(cross(t, [bx, by, bz]));
    let s = if flip != 0 { -1.0 } else { 1.0 };
    Frame { n: [s * n[0], s * n[1], s * n[2]], t, h: s }
}

/// Rotate v by the unit quaternion q = (x, y, z, w).
fn qrot(q: [f64; 4], v: V3) -> V3 {
    let [x, y, z, w] = q;
    let (tx, ty, tz) = (2.0 * (y * v[2] - z * v[1]), 2.0 * (z * v[0] - x * v[2]), 2.0 * (x * v[1] - y * v[0]));
    [v[0] + w * tx + (y * tz - z * ty), v[1] + w * ty + (z * tx - x * tz), v[2] + w * tz + (x * ty - y * tx)]
}

fn unit4(q: [f64; 4]) -> [f64; 4] {
    let mut n = (((0.0 + q[0] * q[0]) + q[1] * q[1]) + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n == 0.0 {
        n = 1.0;
    }
    [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
}

/// (normal, tangent, handed) from one skinned-mesh QTangent (4 x int16 snorm quaternion q = (x, y, z, w) / 32767).
///
/// N = -(q * (0, 0, 1)) (EMPIRICAL: dot > 0.8 with the geometric normal on 92 % of the vertices of f_vgl_flightsuit_02_lod1.skinm).
/// tangent = q * (1, 0, 0), bitangent = q * (0, 1, 0): INFERRED.
/// handed: +1 when B = N x T would reproduce the stored bitangent, -1 when mirrored.
pub fn frame_from_qtangent(raw: [i16; 4]) -> Frame {
    let q = unit4([raw[0] as f64 / 32767.0, raw[1] as f64 / 32767.0, raw[2] as f64 / 32767.0, raw[3] as f64 / 32767.0]);
    let n = qrot(q, [0.0, 0.0, 1.0]);
    let t = qrot(q, [1.0, 0.0, 0.0]);
    let b = qrot(q, [0.0, 1.0, 0.0]);
    let n = [-n[0], -n[1], -n[2]];
    let c = cross(n, t);
    Frame { n, t, h: if c[0] * b[0] + c[1] * b[1] + c[2] * b[2] >= 0.0 { 1.0 } else { -1.0 } }
}

/// Per-vertex frame decoder over a mesh's own streams (decodes on demand: a caller that keeps a subset of the vertices does not
/// pay for the rest).  Static meshes (.cgfm / .cgam, vertex format 4) always carry STREAM_TANGENTS, skinned meshes (.skinm,
/// format 2) always STREAM_QTANGENT, and a float3 normal stream (STREAM_NORMALS_F3) is accepted without tangents (tangent
/// (0, 0, 0), handed 0; EMPIRICAL census of 150 files per extension).  The vectors are in the mesh's own (node-local)
/// space: a caller that moves the vertices by a node matrix rotates these with the same 3x3 (scene builder does).
pub enum FrameReader<'a> {
    Tangents(&'a [u8], Stream),
    QTangent(&'a [u8], Stream),
    NormalsF3(&'a [u8], Stream),
}

impl<'a> FrameReader<'a> {
    pub fn new(m: &Mesh<'a>) -> Option<FrameReader<'a>> {
        if let Some(s) = m.streams.get(&STREAM_TANGENTS) {
            Some(FrameReader::Tangents(m.buf, *s))
        } else if let Some(s) = m.streams.get(&STREAM_QTANGENT) {
            Some(FrameReader::QTangent(m.buf, *s))
        } else {
            m.streams.get(&STREAM_NORMALS_F3).map(|s| FrameReader::NormalsF3(m.buf, *s))
        }
    }

    pub fn get(&self, i: usize) -> Result<Frame> {
        match self {
            FrameReader::Tangents(b, s) => {
                let o = s.off + s.stride * i;
                Ok(frame_from_tangent_words(u32_at(b, o)?, u32_at(b, o + 4)?))
            }
            FrameReader::QTangent(b, s) => {
                let o = s.off + s.stride * i;
                Ok(frame_from_qtangent([i16_at(b, o)?, i16_at(b, o + 2)?, i16_at(b, o + 4)?, i16_at(b, o + 6)?]))
            }
            FrameReader::NormalsF3(b, s) => {
                let o = s.off + s.stride * i;
                Ok(Frame { n: unit(f32s::<3>(b, o)?), t: [0.0; 3], h: 0.0 })
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// .cgf node table (chunk 0x70697fda); the engine's cgf loader visits it once per node
// ---------------------------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct Node {
    pub index: usize,
    pub name: String,
    pub props: String,
    /// 3x4 row-major matrix (rotation | translation) == node transform in the object space (translations sit inside the object
    /// bbox; geometry of the node is stored node-local)
    pub world: [f64; 12],
    /// second 3x4 (parent-relative form, UNCONFIRMED)
    pub local: [f64; 12],
    pub key: u32,
    /// 0/1 build a sub-object, others are helpers
    pub ntype: u8,
    /// node-local bounds
    pub bbox: [f64; 6],
    pub phys_flags: u32,
    pub num_tris: u32,
    pub num_verts: u32,
    pub children: Vec<u16>,
    pub mat_ids: Vec<u16>,
}

fn cstr_list(b: &[u8]) -> Vec<String> {
    b.split(|&c| c == 0).map(|x| String::from_utf8_lossy(x).into_owned()).collect()
}

fn u16_list(data: &[u8], o: usize, n: usize) -> Result<Vec<u16>> {
    (0..n).map(|i| u16_at(data, o + 2 * i)).collect()
}

/// Header (u32): [1]=nodeCount, [3]=u16 child-list length, [4]=u16 material-list length, [5]=name bytes, [6]=property-string
/// bytes, [7]=bytes of the default-transform table.  Nodes (0xd0 B each) start at +0x40, followed by: child u16 list, material
/// u16 list, names (NUL separated, node order), property strings.
pub fn parse_nodes(data: &[u8], chunk: &Chunk) -> Result<Vec<Node>> {
    let base = chunk.offset;
    let mut h = [0usize; 8];
    for (i, v) in h.iter_mut().enumerate() {
        *v = u32_at(data, base + 4 * i)? as usize;
    }
    let (nn, ncl, nml, nname, nprop) = (h[1], h[3], h[4], h[5], h[6]);
    let p0 = base + 0x40;
    let pc = p0 + 0xD0 * nn;
    let pm = pc + 2 * ncl;
    let pn = pm + 2 * nml;
    let pp = pn + nname;
    let children = u16_list(data, pc, ncl)?;
    let mats = u16_list(data, pm, nml)?;
    let names = cstr_list(slice(data, pn, pn + nname));
    let props = cstr_list(slice(data, pp, pp + nprop));
    let mut nodes = Vec::with_capacity(nn);
    let (mut ci, mut mi) = (0usize, 0usize);
    for i in 0..nn {
        let r = p0 + 0xD0 * i;
        let nchild = u16_at(data, r + 0xA4)? as usize;
        let nmat = u8_at(data, r + 0xA7)? as usize;
        let sl = |v: &Vec<u16>, a: usize, n: usize| v[a.min(v.len())..(a + n).min(v.len())].to_vec();
        nodes.push(Node {
            index: i,
            name: names.get(i).cloned().unwrap_or_else(|| format!("node{}", i)),
            props: props.get(i).cloned().unwrap_or_default(),
            world: f32s::<12>(data, r)?,
            local: f32s::<12>(data, r + 0x30)?,
            key: u32_at(data, r + 0x6C)?,
            ntype: u8_at(data, r + 0x76)?,
            bbox: f32s::<6>(data, r + 0x78)?,
            phys_flags: u32_at(data, r + 0x98)?,
            num_tris: u32_at(data, r + 0x9C)?,
            num_verts: u32_at(data, r + 0xA0)?,
            children: sl(&children, ci, nchild),
            mat_ids: sl(&mats, mi, nmat),
        });
        ci += nchild;
        mi += nmat;
    }
    Ok(nodes)
}

pub const IDENT34: [f64; 12] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];
pub const HELPER_NODE_TYPES: [u8; 2] = [2, 3];

/// The matrix that moves mesh vertices assigned to `node` (subset `mat_id >> 16`) into object space: the node's world matrix for
/// a mesh node (type 0 / 1), the identity for a helper node (type 2 / 3).
///
/// Why: a file whose only node is its type-3 root (e.g. flightcntrl_wall_window_b.cgf of the Idris, root rotated 180 deg about Z)
/// tags its whole mesh with that node; applying the root's matrix turned the model around and, with the slanted window frame in
/// it, made it lean through the gym next to it.  The engine builds a sub-object only for node types 0 / 1 (see parse_nodes), so a
/// helper's matrix is never a mesh transform there (INFERRED from that dispatch, not traced to the vertex upload).
/// EMPIRICAL (13 ships + 2 zones, 2,024 files whose subsets name a node with a non-identity matrix):
/// the mesh header's object box (first box, Mesh.bbox_*) equals the raw vertex box, never the transformed one, for the 527 files
/// whose moving nodes are helpers, and the transformed box for 1,027 files with type-0 nodes.
pub fn geometry_matrix(node: Option<&Node>) -> [f64; 12] {
    match node {
        None => IDENT34,
        Some(n) if HELPER_NODE_TYPES.contains(&n.ntype) => IDENT34,
        Some(n) => n.world,
    }
}

pub fn xform(m: &[f64; 12], p: V3) -> V3 {
    [
        m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3],
        m[4] * p[0] + m[5] * p[1] + m[6] * p[2] + m[7],
        m[8] * p[0] + m[9] * p[1] + m[10] * p[2] + m[11],
    ]
}

/// The frame with the 3x3 part of the 3x4 matrix m applied to the vectors and re-normalised (non-uniform scale is ignored; node
/// matrices of the models are rigid).  A zero tangent (no tangent stream) stays zero.  A mirroring matrix flips the handedness
/// (the cross product of two rotated vectors is mirrored).
pub fn rotate_frame(m: &[f64; 12], f: Frame) -> Frame {
    let rot = |v: V3| unit([m[0] * v[0] + m[1] * v[1] + m[2] * v[2], m[4] * v[0] + m[5] * v[1] + m[6] * v[2], m[8] * v[0] + m[9] * v[1] + m[10] * v[2]]);
    let nn = rot(f.n);
    if f.h == 0.0 {
        return Frame { n: nn, t: f.t, h: f.h };
    }
    let tt = rot(f.t);
    let det = m[0] * (m[5] * m[10] - m[6] * m[9]) - m[1] * (m[4] * m[10] - m[6] * m[8]) + m[2] * (m[4] * m[9] - m[5] * m[8]);
    Frame { n: nn, t: tt, h: if det < 0.0 { -f.h } else { f.h } }
}

// ---------------------------------------------------------------------------------------------
// physics chunk 0x58de1772 (groups of physics geometry)
// ---------------------------------------------------------------------------------------------
pub fn group_name(g: u32) -> &'static str {
    match g {
        0 => "geometry",
        4 => "clustermesh",
        5 => "bitvoxel",
        7 => "distance field",
        _ => "?",
    }
}
pub const PHYS_GEOM: u32 = 0;
pub const PHYS_CLUSTER: u32 = 4;
pub const PHYS_FIELD: u32 = 7;

#[derive(Debug, Clone)]
pub struct PhysEntry {
    pub group: u32,
    pub index: u32,
    pub offset: usize,
    pub size: usize,
    pub node: u16,
    pub proxy: u16,
}

/// Header: u32 count[10] at +0, u32 total at +0x28, entries {u32 size, u16 nodeIndex(0xffff = whole object), u16 proxyIndex(+1; 0 =
/// none)} from +0x50 (8 B each); blobs follow back to back in entry order (group 0 first).  Physicalize_0x900 hands each blob to a
/// loader chosen by group number.
pub fn parse_physics_groups(data: &[u8], chunk: &Chunk) -> Result<Vec<PhysEntry>> {
    let base = chunk.offset;
    let mut cnt = [0u32; 10];
    for (i, c) in cnt.iter_mut().enumerate() {
        *c = u32_at(data, base + 4 * i)?;
    }
    let total = u32_at(data, base + 0x28)? as usize;
    let mut ents = Vec::with_capacity(total.min(1 << 20));
    for i in 0..total {
        let o = base + 0x50 + 8 * i;
        ents.push((u32_at(data, o)? as usize, u16_at(data, o + 4)?, u16_at(data, o + 6)?));
    }
    let mut pos = base + 0x50 + 8 * total;
    let mut out = Vec::new();
    let mut k = 0;
    for g in 0..10u32 {
        for i in 0..cnt[g as usize] {
            let (sz, node, proxy) = *ents.get(k).ok_or_else(|| Error::msg("IndexError: list index out of range"))?;
            k += 1;
            out.push(PhysEntry { group: g, index: i, offset: pos, size: sz, node, proxy });
            pos += sz;
        }
    }
    Ok(out)
}

/// Decoded collision geometry (geometry-local coordinates).
#[derive(Debug, Clone)]
pub enum GeomKind {
    Box { basis: [f64; 9], oriented: u32, center: V3, size: V3 },
    Sphere { center: V3, radius: f64 },
    /// capsule = false: cylinder
    Cyl { center: V3, axis: V3, radius: f64, half_height: f64, capsule: bool },
    Mesh { verts: Vec<V3>, tris: Vec<[u32; 3]> },
    Cluster { verts: Vec<V3>, tris: Vec<[u32; 3]> },
    Unsupported(u32),
}

#[derive(Debug, Clone)]
pub struct Geom {
    pub kind: GeomKind,
    /// per-triangle bytes of a trimesh / cluster mesh (sub-material index of the cgf's material), when present
    pub mat: Option<Vec<u8>>,
    /// the geometry header's surface index (parse_phys_geometry); None for cluster meshes / distance fields
    pub surface_idx: Option<u32>,
}

impl Geom {
    pub fn kind_name(&self) -> String {
        match &self.kind {
            GeomKind::Box { .. } => "box".into(),
            GeomKind::Sphere { .. } => "sphere".into(),
            GeomKind::Cyl { capsule, .. } => if *capsule { "capsule" } else { "cylinder" }.into(),
            GeomKind::Mesh { .. } => "mesh".into(),
            GeomKind::Cluster { .. } => "cluster".into(),
            GeomKind::Unsupported(t) => format!("unsupported({})", t),
        }
    }
}

fn verr(s: String) -> Error {
    Error::Msg(format!("ValueError: {}", s))
}

/// A physics geometry: u32 version=2, a 0x40-byte header ([4:16] inertia diag, [16:32] quaternion, [32:44] origin(COM), [44:48]
/// volume), then the geometry data: u32 type and the type's own layout (see per type).
pub fn parse_phys_geometry(data: &[u8], off: usize) -> Result<Geom> {
    let ver = u32_at(data, off)?;
    if ver != 2 {
        return Err(verr(format!("phys geometry version {}", ver)));
    }
    // header +0x34 (u32): the geometry's surface index (CONFIRMED offset; name INFERRED).  EMPIRICAL (800 random spaceship models, 4,180 group-0 geometries): it is a sub-material
    // index of the cgf's own material: on 645 of 649 trimeshes with per-triangle bytes it equals the commonest triangle byte; 0 on
    // 99 % of primitives; 0xffffffff on 'allow_no_collide' local-grid meshes.  Whether the physics uses it as the part's material
    // is INFERRED (consumer not read).
    let surface_idx = u32_at(data, off + 4 + 0x34)?;
    let mut p = off + 4 + 0x40;
    let t = u32_at(data, p)?;
    p += 4;
    let mut mat = None;
    let kind = match t {
        // box: embedded primitive box, 0x44 B: Basis 3x3 (row-major floats),
        // int bOriented, center(3f), size(3f) (half extents), u32 (=1).  Size verified from the 140-byte box blobs.
        0 => GeomKind::Box { basis: f32s::<9>(data, p)?, oriented: u32_at(data, p + 36)?, center: f32s::<3>(data, p + 40)?, size: f32s::<3>(data, p + 52)? },
        // sphere: center(3f) r(f) then the bounding primitive box
        4 => GeomKind::Sphere { center: f32s::<3>(data, p)?, radius: f32_at(data, p + 12)? },
        // cylinder / capsule: center(3f) axis(3f) r(f) hh(f), u32 (=0x12), box
        // EMPIRICAL: the field order was swapped (hh, r) until checked against the render meshes of 92 single-cylinder models:
        // median bounding-box error 0.248 m as (hh, r), 0.009 m as (r, hh); e.g. ext_delta_pipe_strght_80m_a is 80 m long with
        // radius 4.45 and the old order made it 8.9 m long and 80 m wide.  Capsule (only 6 clean samples, error 2.3 m vs 94 m
        // swapped): same order assumed; whether hh excludes the end caps is UNVERIFIED (tess_cyl adds r beyond hh).
        5 | 6 => {
            let v = f32s::<8>(data, p)?;
            GeomKind::Cyl { center: [v[0], v[1], v[2]], axis: [v[3], v[4], v[5]], radius: v[6], half_height: v[7], capsule: t == 6 }
        }
        // triangle mesh (everything inline)
        1 => {
            let nv = u32_at(data, p)? as usize;
            let nt = u32_at(data, p + 4)? as usize;
            let flags = u32_at(data, p + 12)?;
            let nremap = u32_at(data, p + 16)? as usize;
            p += 20;
            let has_ids = u8_at(data, p)?;
            let has_mat = u8_at(data, p + 1)?;
            p += 2;
            if nremap != 0 {
                p += 2 * nremap; // vertex remap (render-mesh vertex -> collision vertex)
            }
            if has_ids != 0 {
                p += 2 * nt; // per-triangle source triangle id
            }
            if has_mat != 0 {
                mat = Some(slice(data, p, p + nt).to_vec());
                p += nt;
            }
            if flags & 1 != 0 || flags & 2 != 0 {
                return Err(verr(format!("external vertex/index mesh (flags={:#x}) not supported", flags)));
            }
            let verts = (0..nv).map(|i| f32s::<3>(data, p + 12 * i)).collect::<Result<Vec<_>>>()?;
            p += 12 * nv;
            let idx = (0..3 * nt).map(|i| u16_at(data, p + 2 * i)).collect::<Result<Vec<_>>>()?;
            let tris = idx.chunks_exact(3).map(|c| [c[0] as u32, c[1] as u32, c[2] as u32]).collect();
            GeomKind::Mesh { verts, tris }
        }
        _ => GeomKind::Unsupported(t),
    };
    Ok(Geom { kind, mat, surface_idx: Some(surface_idx) })
}

/// Cluster mesh (version -2 layout).
/// u32 0xCAFEBABE, u32 blockSize=0x100, u32 treeVersion=0, i64 version(-2), i64 n0..n4, i64 hasTree, 3 doubles (vertex scale), 3
/// doubles (vertex origin), then arrays: vertices[n0] 8 B (4 x int16, w unused), triangle corner indices[n1] u8 (cluster-local),
/// per-triangle bytes[n2], clusters[n3] 16 B {u32 firstVertex, u32 firstIndex, u32 firstTriangle, u16 numIndices, u16
/// numVertices}, n4 x u32, tree.  The descriptor/vertex field meanings are EMPIRICAL (continuity checks hold on all samples).
pub fn parse_cluster_mesh(data: &[u8], off: usize) -> Result<Geom> {
    let magic = u32_at(data, off)?;
    let ver = i64_at(data, off + 12)?;
    if magic != 0xCAFEBABE || ver != -2 {
        return Err(verr(format!("cluster mesh magic/version {:#x}/{}", magic, ver)));
    }
    let n0 = i64_at(data, off + 20)? as usize;
    let n1 = i64_at(data, off + 28)? as usize;
    let n2 = i64_at(data, off + 36)? as usize;
    let n3 = i64_at(data, off + 44)? as usize;
    let scale = [f64_at(data, off + 68)?, f64_at(data, off + 76)?, f64_at(data, off + 84)?];
    let org = [f64_at(data, off + 92)?, f64_at(data, off + 100)?, f64_at(data, off + 108)?];
    let pv = off + 116;
    let pi = pv + 8 * n0;
    let pt = pi + n1;
    let pc = pt + n2;
    let mat = slice(data, pt, pt + n2).to_vec();
    let mut verts = Vec::with_capacity(n0);
    for i in 0..n0 {
        let o = pv + 8 * i;
        verts.push([
            org[0] + scale[0] * i16_at(data, o)? as f64,
            org[1] + scale[1] * i16_at(data, o + 2)? as f64,
            org[2] + scale[2] * i16_at(data, o + 4)? as f64,
        ]);
    }
    let mut tris = Vec::new();
    for c in 0..n3 {
        let o = pc + 16 * c;
        let vs = u32_at(data, o)?;
        let is = u32_at(data, o + 4)? as usize;
        let nidx = u16_at(data, o + 12)? as usize;
        for t in 0..nidx / 3 {
            let mut tri = [0u32; 3];
            for (k, v) in tri.iter_mut().enumerate() {
                *v = vs + u8_at(data, pi + is + 3 * t + k).map_err(|_| Error::msg("IndexError: index out of range"))? as u32;
            }
            tris.push(tri);
        }
    }
    Ok(Geom { kind: GeomKind::Cluster { verts, tris }, mat: Some(mat), surface_idx: None })
}

/// Rotate v by the quaternion q = (x, y, z, w), normalising it first.
fn quat_rotate(q: [f64; 4], v: V3) -> V3 {
    let mut n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).powf(0.5);
    if n == 0.0 {
        n = 1.0;
    }
    qrot([q[0] / n, q[1] / n, q[2] / n, q[3] / n], v)
}

/// Physics group 7: a dense distance field blob (geometry type 0x18).  Returns the closed triangle mesh stored in the blob (the
/// engine's remesh of the field) in object space; the distance grid itself is not iso-surfaced (its
/// values are non-negative and the engine subtracts a run-time offset that is not in the file).
/// Layout (little-endian, offsets in the blob): u32 0xdeadbeb5; u32 type 0x18; f32 (0.01, meaning unknown); i32[3] cells
/// nx,ny,nz at 12; f32 cell size at 24; f32[4] quaternion x,y,z,w at 28; f32[3] position at 44; i64[3] dims at 56; f32
/// grid[dx*dy*dz] at 80 (EMPIRICAL: the padded dims, not the cell counts); u32 hasCube (+ 0xdeadbeef surfel cube, skipped by
/// size); u32 hasMesh; u32 nV, nV x vec3 f32; u32 nI, nI x u32 triangle indices; BVH and padding after that are ignored.
/// Confirmed in the engine: magic/type, field order, the cube sizes.  EMPIRICAL (6 blobs, closed meshes V-E+F=2): the mesh
/// parse, and the pose p_obj = position + R(quat) * p_local.  Winding is unchecked.  The mesh is a coarse hull 0.3-0.5 m outside
/// the proxy meshes of the same node.
pub fn parse_distance_field(data: &[u8], off: usize, size: usize) -> Result<Geom> {
    let magic = u32_at(data, off)?;
    let gtype = u32_at(data, off + 4)?;
    if magic != 0xDEADBEB5 || gtype != 0x18 {
        return Err(verr(format!("distance field magic/type {:#x}/{:#x}", magic, gtype)));
    }
    let (dx, dy, dz) = (i64_at(data, off + 56)?, i64_at(data, off + 64)?, i64_at(data, off + 72)?);
    let quat = f32s::<4>(data, off + 28)?;
    let pos = f32s::<3>(data, off + 44)?;
    let mut p = off + 80 + (4 * dx * dy * dz) as usize;
    let has_cube = u32_at(data, p)?;
    p += 4;
    if has_cube != 0 {
        let magic2 = u32_at(data, p)?;
        if magic2 != 0xDEADBEEF {
            return Err(verr(format!("surfel cube magic {:#x}", magic2)));
        }
        let a = u32_at(data, p + 28)? as usize;
        let b = u32_at(data, p + 32)? as usize;
        p += 36 + 6 * (4 + 16 * a * b);
    }
    let has_mesh = u32_at(data, p)?;
    if has_mesh == 0 {
        return Err(verr("distance field without a stored mesh".into()));
    }
    let nv = u32_at(data, p + 4)? as usize;
    p += 8;
    let raw = (0..3 * nv).map(|i| f32_at(data, p + 4 * i)).collect::<Result<Vec<_>>>()?;
    p += 12 * nv;
    let ni = u32_at(data, p)? as usize;
    let idx = (0..ni).map(|i| u32_at(data, p + 4 + 4 * i)).collect::<Result<Vec<_>>>()?;
    if ni % 3 != 0 || idx.iter().max().map_or(false, |&m| m as usize >= nv) || p + 4 + 4 * ni > off + size {
        return Err(verr(format!("distance field mesh counts out of range (V={}, I={})", nv, ni)));
    }
    let verts = (0..nv)
        .map(|i| {
            let r = quat_rotate(quat, [raw[3 * i], raw[3 * i + 1], raw[3 * i + 2]]);
            [pos[0] + r[0], pos[1] + r[1], pos[2] + r[2]]
        })
        .collect();
    let tris = idx.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
    Ok(Geom { kind: GeomKind::Mesh { verts, tris }, mat: None, surface_idx: None })
}

pub struct Cgf {
    pub nodes: Vec<Node>,
    /// (entry, geometry, error text)
    pub physics: Vec<(PhysEntry, Option<Geom>, Option<String>)>,
}

/// Parse a .cgf/.cga: nodes and physics.  `physics = false` skips the collision geometry (cluster meshes alone were ~20 s of a
/// 400k-instance zone compile) for callers that only need the node table, e.g. bounding spheres.
pub fn parse_cgf(data: &[u8], physics: bool) -> Result<Cgf> {
    let f = IvoFile::new(data)?;
    let nodes = match f.get(CH_CGF_NODES) {
        Some(nc) => parse_nodes(data, nc)?,
        None => vec![],
    };
    let mut out = vec![];
    if let (Some(pc), true) = (f.get(CH_CGF_PHYSICS), physics) {
        for e in parse_physics_groups(data, pc)? {
            let r = match e.group {
                PHYS_GEOM => Some(parse_phys_geometry(data, e.offset)),
                PHYS_CLUSTER => Some(parse_cluster_mesh(data, e.offset)),
                PHYS_FIELD => Some(parse_distance_field(data, e.offset, e.size)),
                _ => None,
            };
            let (g, err) = match r {
                Some(Ok(g)) => (Some(g), None),
                // keep going, report in the viewer (the message is prefixed with the error class name)
                Some(Err(Error::Msg(m))) => (None, Some(if m.starts_with("ValueError") || m.starts_with("IndexError") { m } else { format!("error: {}", m) })),
                Some(Err(Error::NotIvo(m))) => (None, Some(m)),
                None => (None, Some(format!("group {} ({}) not decoded", e.group, group_name(e.group)))),
            };
            out.push((e, g, err));
        }
    }
    Ok(Cgf { nodes, physics: out })
}
