//! DDS parsing and decoding of BC1/BC2/BC3/BC4/BC5 and plain 8-bit formats to RGBA.
//!
//! BC7 (DXGI 98/99) is decoded by `crate::bc7` (all 8 modes); browsers that have EXT_texture_compression_bptc still get the raw
//! blocks from the texture servers, everyone else a PNG.  The sRGB flag does not change the output (stored values, as BC1-3).
//! BC6H (DXGI 95 / 96, HDR) is decoded by `crate::bc6h` and tone-mapped to 8-bit LDR for the PNG paths (never sent raw).  The BC alpha-coverage helpers (used by the registry's `acov`)
//! are at the end.  The block decoders run in parallel over block rows (rayon).

use rayon::prelude::*;

use crate::error::*;

pub const DXGI_BC1: u32 = 71;
pub const DXGI_BC1_SRGB: u32 = 72;
pub const DXGI_BC2: u32 = 74;
pub const DXGI_BC2_SRGB: u32 = 75;
pub const DXGI_BC3: u32 = 77;
pub const DXGI_BC3_SRGB: u32 = 78;
pub const DXGI_BC7: u32 = 98;
pub const DXGI_BC7_SRGB: u32 = 99;

/// `block`: pixels per block edge (1 = plain pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Format {
    pub name: &'static str,
    pub block: usize,
    pub bytes: usize,
    pub srgb: bool,
}

const fn fmt(name: &'static str, block: usize, bytes: usize, srgb: bool) -> Option<Format> {
    Some(Format { name, block, bytes, srgb })
}

/// Format(name, block, bytes, srgb) for a DXGI code, or None when the format is not known here.
///
/// CryEngine reads header DXGI 81 / 84 (UNORM in Microsoft's numbering) as the SIGNED BC4S / BC5S formats and 80 / 83 (TYPELESS)
/// as the unsigned ones (CONFIRMED against the engine and on real ddna files; 81 = BC4S is from the engine only).
pub fn format_info(dxgi: u32) -> Option<Format> {
    match dxgi {
        71 => fmt("bc1", 4, 8, false),
        72 => fmt("bc1", 4, 8, true),
        74 => fmt("bc2", 4, 16, false),
        75 => fmt("bc2", 4, 16, true),
        77 => fmt("bc3", 4, 16, false),
        78 => fmt("bc3", 4, 16, true),
        80 => fmt("bc4", 4, 8, false),
        81 => fmt("bc4s", 4, 8, false),
        83 => fmt("bc5", 4, 16, false),
        84 => fmt("bc5s", 4, 16, false),
        94 | 95 => fmt("bc6h", 4, 16, false),
        96 => fmt("bc6hs", 4, 16, false),
        98 => fmt("bc7", 4, 16, false),
        99 => fmt("bc7", 4, 16, true),
        28 => fmt("rgba8", 1, 4, false),
        29 => fmt("rgba8", 1, 4, true),
        87 => fmt("bgra8", 1, 4, false),
        91 => fmt("bgra8", 1, 4, true),
        61 => fmt("r8", 1, 1, false),
        65 => fmt("a8", 1, 1, false),
        49 => fmt("rg8", 1, 2, false),
        93 => fmt("bgrx8", 1, 4, true),
        56 => fmt("r16", 1, 2, false),
        35 => fmt("rg16", 1, 4, false),
        DXGI_PRIVATE_BGR8 => fmt("bgr8", 1, 3, false),
        _ => None,
    }
}

pub const DECODABLE: [&str; 19] = ["bc1", "bc2", "bc3", "bc7", "bc6h", "bc6hs", "a8", "rg8", "bgrx8", "r16", "rg16", "bgr8", "bc4", "bc4s", "bc5", "bc5s", "rgba8", "bgra8", "r8"];

fn c565(c: u16) -> [u8; 3] {
    let (r, g, b) = ((c >> 11) & 31, (c >> 5) & 63, c & 31);
    [((r << 3) | (r >> 2)) as u8, ((g << 2) | (g >> 4)) as u8, ((b << 3) | (b >> 2)) as u8]
}

fn bc1_colors(c0: u16, c1: u16, punch_through: bool) -> [[u8; 4]; 4] {
    let (a, b) = (c565(c0), c565(c1));
    let mix = |wa: u32, wb: u32, d: u32| -> [u8; 4] {
        [((wa * a[0] as u32 + wb * b[0] as u32) / d) as u8, ((wa * a[1] as u32 + wb * b[1] as u32) / d) as u8, ((wa * a[2] as u32 + wb * b[2] as u32) / d) as u8, 255]
    };
    if c0 > c1 || !punch_through {
        [[a[0], a[1], a[2], 255], [b[0], b[1], b[2], 255], mix(2, 1, 3), mix(1, 2, 3)]
    } else {
        [[a[0], a[1], a[2], 255], [b[0], b[1], b[2], 255], mix(1, 1, 2), [0, 0, 0, 0]]
    }
}

/// The 8-value palette of a BC3 alpha block / BC4 channel (a0 > a1: 8 values, else 6 plus 0 and 255).
fn bc4_palette(a0: u8, a1: u8) -> [u8; 8] {
    let (a0i, a1i) = (a0 as u32, a1 as u32);
    let mut pal = [a0, a1, 0, 0, 0, 0, 0, 0];
    if a0 > a1 {
        for i in 1..7u32 {
            pal[1 + i as usize] = (((7 - i) * a0i + i * a1i) / 7) as u8;
        }
    } else {
        for i in 1..5u32 {
            pal[1 + i as usize] = (((5 - i) * a0i + i * a1i) / 5) as u8;
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    pal
}

/// The 16 texel values (0..255) of one 8-byte BC4 block (same interpolation as the BC3 alpha block).
fn bc4_values(blk: &[u8]) -> [u8; 16] {
    let pal = bc4_palette(blk[0], blk[1]);
    let mut bits = 0u64;
    for k in 0..6 {
        bits |= (blk[2 + k] as u64) << (8 * k);
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = pal[((bits >> (3 * i)) & 7) as usize];
    }
    out
}

/// The 16 texel values (-1..1) of one 8-byte signed BC4 block: endpoints are int8, the 6-interpolant mode adds the fixed values
/// -127 / 127, and results are divided by 127 and clamped.
fn bc4s_values(blk: &[u8]) -> [f64; 16] {
    let (r0, r1) = (blk[0] as i8 as f64, blk[1] as i8 as f64);
    let mut pal = [r0, r1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    if r0 > r1 {
        for i in 1..7 {
            pal[1 + i] = ((7 - i) as f64 * r0 + i as f64 * r1) / 7.0;
        }
    } else {
        for i in 1..5 {
            pal[1 + i] = ((5 - i) as f64 * r0 + i as f64 * r1) / 5.0;
        }
        pal[6] = -127.0;
        pal[7] = 127.0;
    }
    let mut bits = 0u64;
    for k in 0..6 {
        bits |= (blk[2 + k] as u64) << (8 * k);
    }
    let mut out = [0.0; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (pal[((bits >> (3 * i)) & 7) as usize] / 127.0).clamp(-1.0, 1.0);
    }
    out
}

/// Decode a block format row by row: `block_fn(blk, px)` fills the 16 RGBA texels of one block (row-major 4x4).
fn decode_blocks(data: &[u8], w: usize, h: usize, bs: usize, alpha_opaque: bool, block_fn: impl Fn(&[u8], &mut [[u8; 4]; 16]) + Sync) -> Vec<u8> {
    let (bw, bh) = (((w + 3) / 4).max(1), ((h + 3) / 4).max(1));
    let mut out = vec![0u8; w * h * 4];
    if alpha_opaque {
        out.par_chunks_mut(4).for_each(|p| p[3] = 255);
    }
    if w == 0 || h == 0 {
        return out;
    }
    out.par_chunks_mut(w * 4 * 4).enumerate().for_each(|(by, rows)| {
        if by >= bh {
            return;
        }
        let nrows = rows.len() / (w * 4);
        let mut px = [[0u8; 4]; 16];
        for bx in 0..bw {
            let pos = (by * bw + bx) * bs;
            let Some(blk) = data.get(pos..pos + bs) else { return };
            block_fn(blk, &mut px);
            for i in 0..16 {
                let (x, y) = (bx * 4 + (i & 3), i >> 2);
                if x >= w || y >= nrows {
                    continue;
                }
                rows[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px[i]);
            }
        }
    });
    out
}

/// `fmt` in 'bc1', 'bc2', 'bc3', 'bc7'.  Returns RGBA bytes (w*h*4); missing trailing blocks stay zero.
pub fn decode_bc(fmt: &str, data: &[u8], w: usize, h: usize) -> Vec<u8> {
    match fmt {
        "bc1" => decode_blocks(data, w, h, 8, false, |blk, px| {
            let cols = bc1_colors(u16::from_le_bytes([blk[0], blk[1]]), u16::from_le_bytes([blk[2], blk[3]]), true);
            let bits = u32::from_le_bytes([blk[4], blk[5], blk[6], blk[7]]);
            for i in 0..16 {
                px[i] = cols[((bits >> (2 * i)) & 3) as usize];
            }
        }),
        "bc2" | "bc3" => {
            let bc2 = fmt == "bc2";
            decode_blocks(data, w, h, 16, false, move |blk, px| {
                let cols = bc1_colors(u16::from_le_bytes([blk[8], blk[9]]), u16::from_le_bytes([blk[10], blk[11]]), false);
                let bits = u32::from_le_bytes([blk[12], blk[13], blk[14], blk[15]]);
                let mut alpha = [0u8; 16];
                if bc2 {
                    let a64 = u64::from_le_bytes(blk[0..8].try_into().unwrap());
                    for (i, a) in alpha.iter_mut().enumerate() {
                        *a = (((a64 >> (4 * i)) & 15) * 17) as u8;
                    }
                } else {
                    alpha = bc4_values(&blk[0..8]);
                }
                for i in 0..16 {
                    let mut c = cols[((bits >> (2 * i)) & 3) as usize];
                    c[3] = alpha[i];
                    px[i] = c;
                }
            })
        }
        "bc7" => decode_blocks(data, w, h, 16, false, |blk, px| crate::bc7::decode_block(blk, px)),
        "bc6h" | "bc6hs" => {
            let signed = fmt == "bc6hs";
            decode_blocks(data, w, h, 16, true, move |blk, px| crate::bc6h::decode_block_ldr(blk, signed, px))
        }
        _ => panic!("decode_bc: {}", fmt),
    }
}

/// RGBA bytes (w*h*4) for a decodable format name (DECODABLE).  BC4 and R8 are shown as grey, BC5 as R,G with B = 0, all opaque;
/// BC4S is grey and BC5S is a tangent-space normal (x, y from the blocks, z = sqrt(1 - x^2 - y^2), each stored as
/// (v * 0.5 + 0.5) * 255); BGRA8 is swizzled to RGBA.  Fails for anything else.
pub fn decode(fmt: &str, data: &[u8], w: usize, h: usize) -> Result<Vec<u8>> {
    match fmt {
        "bc1" | "bc2" | "bc3" | "bc7" | "bc6h" | "bc6hs" => Ok(decode_bc(fmt, data, w, h)),
        "rgba8" | "bgra8" => {
            let n = w * h * 4;
            let mut out = vec![0u8; n];
            let m = n.min(data.len());
            out[..m].copy_from_slice(&data[..m]);
            if fmt == "bgra8" {
                out.chunks_exact_mut(4).for_each(|p| p.swap(0, 2));
            }
            Ok(out)
        }
        "r8" | "a8" => {
            let mut out = vec![0u8; w * h * 4];
            for i in 0..w * h {
                let v = data.get(i).copied().unwrap_or(0);
                out[4 * i..4 * i + 4].copy_from_slice(&[v, v, v, 255]);
            }
            Ok(out)
        }
        // plain channels shown as they are, opaque: RG8 and RG16 as R, G with B = 0, R16 as grey (the high byte of each 16-bit value),
        // BGRX8 / BGR8 swizzled to RGB
        "rg8" | "rg16" | "r16" | "bgrx8" | "bgr8" => {
            let (bytes, take): (usize, fn(&[u8]) -> [u8; 3]) = match fmt {
                "rg8" => (2, |p| [p[0], p[1], 0]),
                "rg16" => (4, |p| [p[1], p[3], 0]),
                "r16" => (2, |p| [p[1], p[1], p[1]]),
                "bgrx8" => (4, |p| [p[2], p[1], p[0]]),
                _ => (3, |p| [p[2], p[1], p[0]]),
            };
            let mut out = vec![0u8; w * h * 4];
            for (i, px) in out.chunks_exact_mut(4).enumerate() {
                if let Some(p) = data.get(i * bytes..(i + 1) * bytes) {
                    let c = take(p);
                    px.copy_from_slice(&[c[0], c[1], c[2], 255]);
                }
            }
            Ok(out)
        }
        "bc4s" | "bc5s" => {
            let bc5 = fmt == "bc5s";
            Ok(decode_blocks(data, w, h, if bc5 { 16 } else { 8 }, true, move |blk, px| {
                let xs = bc4s_values(&blk[..8]);
                let ys = if bc5 { bc4s_values(&blk[8..]) } else { xs };
                for i in 0..16 {
                    let (x, y) = (xs[i], ys[i]);
                    let q = |v: f64| py_round((v * 0.5 + 0.5) * 255.0) as u8;
                    px[i] = if bc5 {
                        let z = f64::max(0.0, 1.0 - x * x - y * y).powf(0.5);
                        [q(x), q(y), q(z), 255]
                    } else {
                        let g = q(x);
                        [g, g, g, 255]
                    };
                }
            }))
        }
        "bc4" | "bc5" => {
            let bc5 = fmt == "bc5";
            Ok(decode_blocks(data, w, h, if bc5 { 16 } else { 8 }, true, move |blk, px| {
                let red = bc4_values(&blk[..8]);
                let green = if bc5 { bc4_values(&blk[8..]) } else { red };
                for i in 0..16 {
                    px[i] = [red[i], green[i], if bc5 { 0 } else { red[i] }, 255];
                }
            }))
        }
        _ => Err(Error::msg(format!("cannot decode {} here", fmt))),
    }
}

/// Private pseudo-DXGI code for packed 24-bit B8G8R8 (no DXGI equivalent).
pub const DXGI_PRIVATE_BGR8: u32 = 1000;

/// DXGI code of a legacy (no FourCC, no DX10 header) uncompressed pixel format, from its flags / channel masks, or None.
/// EMPIRICAL (survey of the archive's .dds without DXGI code):
/// * flags 0x41 / 0x40, 32 bits, R 0x00ff0000 G 0x0000ff00 B 0x000000ff (A 0xff000000 or none): B8G8R8A8 in memory (Illum detail maps
///   rust_metal_detail.dds, paint_b_detail.dds, Textures/defaults/grey.dds); the same with R and B swapped is R8G8B8A8;
/// * flags 0x40, 24 bits, the same BGR masks: packed B8G8R8 (212 files, e.g. Engine/EngineAssets/Textures/screen_noisy_bump.dds);
/// * flags 0x40, 16 bits, R 0xffff: R16 UNORM (160 files, planet terrain elevation maps, mountain_cells_04_elev.dds);
/// * flags 0x40, 32 bits, R 0xffff G 0xffff0000: R16G16 UNORM (stanton1_clouds_wind.dds);
/// * flags 0x2 (alpha only), 8 bits: A8 (cloak_transition.dds); flags 0x20000 (luminance), 8 bits: L8 (SearchTex.dds).
fn legacy_pixel(d: &[u8]) -> Option<u32> {
    if d.len() < 128 {
        return None;
    }
    let flags = u32_at(d, 80).ok()?;
    let bits = u32_at(d, 88).ok()?;
    let (r, g, b) = (u32_at(d, 92).ok()?, u32_at(d, 96).ok()?, u32_at(d, 100).ok()?);
    if flags & 0x20000 != 0 && bits == 8 {
        return Some(61);
    }
    if flags & 0x2 != 0 && flags & 0x40 == 0 && bits == 8 {
        return Some(65);
    }
    if flags & 0x40 == 0 {
        return None;
    }
    match (bits, r, g, b) {
        (32, 0xFF0000, 0xFF00, 0xFF) => Some(87),
        (32, 0xFF, 0xFF00, 0xFF0000) => Some(28),
        (24, 0xFF0000, 0xFF00, 0xFF) => Some(DXGI_PRIVATE_BGR8),
        (16, 0xFFFF, 0, 0) => Some(56),
        (32, 0xFFFF, 0xFFFF0000, 0) => Some(35),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub width: usize,
    pub height: usize,
    pub mips: usize,
    pub dxgi: Option<u32>,
    pub header_size: usize,
}

/// Header facts, or None if `d` is not a DDS file.
pub fn parse_header(d: &[u8]) -> Option<Header> {
    if d.len() < 128 || &d[..4] != b"DDS " {
        return None;
    }
    let height = u32_at(d, 12).ok()? as usize;
    let width = u32_at(d, 16).ok()? as usize;
    let mips = u32_at(d, 28).ok()? as usize;
    let fourcc = &d[84..88];
    let (dxgi, hdr) = if fourcc == b"DX10" {
        (Some(u32_at(d, 128).ok()?), 148)
    } else {
        let mut dx = match fourcc {
            b"DXT1" => Some(DXGI_BC1),
            b"DXT3" => Some(DXGI_BC2),
            b"DXT5" => Some(DXGI_BC3),
            b"ATI1" | b"BC4U" => Some(80),
            b"ATI2" | b"BC5U" => Some(83),
            _ => None,
        };
        if dx.is_none() && fourcc == b"\0\0\0\0" {
            dx = legacy_pixel(d);
        }
        (dx, 128)
    };
    Some(Header { width, height, mips: mips.max(1), dxgi, header_size: hdr })
}

/// Number of depth slices of the base level: > 1 for a volume (3D) texture, else 1.  A volume has the DDSD_DEPTH flag (0x800000)
/// / the DDSCAPS2_VOLUME caps2 bit (0x200000) and a depth at offset 24, or a DX10 header whose resource dimension is 3
/// (TEXTURE3D, value 4).  EMPIRICAL (VolumetricClouds/NoiseShapePacked_LowHighFreq.dds: 128 x 128 x 128 BC5, 6 mips; each mip
/// has max(1, depth >> level) slices, which is exactly the file's payload size).
pub fn parse_depth(d: &[u8]) -> usize {
    if d.len() < 128 || &d[..4] != b"DDS " {
        return 1;
    }
    let flags = u32_at(d, 8).unwrap_or(0);
    let depth = u32_at(d, 24).unwrap_or(0) as usize;
    let caps2 = u32_at(d, 112).unwrap_or(0);
    let mut vol = flags & 0x800000 != 0 || caps2 & 0x200000 != 0;
    if &d[84..88] == b"DX10" && d.len() >= 148 && u32_at(d, 132).unwrap_or(0) == 4 {
        vol = true;
    }
    if vol {
        depth.max(1)
    } else {
        1
    }
}

/// Number of faces of a cube map (6, or the number of face bits set in caps2), else 1.  A cube map has DDSCAPS2_CUBEMAP (0x200) and
/// the face bits 0x400 (+X) .. 0x8000 (-Z) in caps2 at offset 112, or a DX10 header whose miscFlag (offset 136) has the TEXTURECUBE
/// bit (4).  EMPIRICAL (Data/Materials/VFX/TerraPrime/Blocks_Monorail/Monorail_Cubemap_cm.dds: caps2 0xFE00, DX10 miscFlag 4, 256 x 256
/// BC6H, 7 mips; each side file `.dds.N` holds the six faces of one mip one after another, but the base file stores the remaining
/// small mips face by face; the face order +X -X +Y -Y +Z -Z is the D3D one, INFERRED).
pub fn parse_faces(d: &[u8]) -> usize {
    if d.len() < 128 || &d[..4] != b"DDS " {
        return 1;
    }
    let caps2 = u32_at(d, 112).unwrap_or(0);
    let mut cube = caps2 & 0x200 != 0;
    if &d[84..88] == b"DX10" && d.len() >= 148 && u32_at(d, 132).unwrap_or(0) == 3 {
        cube |= u32_at(d, 136).unwrap_or(0) & 4 != 0;
    }
    if !cube {
        return 1;
    }
    match (caps2 & 0xFC00).count_ones() {
        0 => 6,
        n => n as usize,
    }
}

/// ('bc1'|'bc2'|'bc3'|'bc7', bytes per 4x4 block) or None for unsupported formats.
pub fn block_format(dxgi: u32) -> Option<(&'static str, usize)> {
    match dxgi {
        DXGI_BC1 | DXGI_BC1_SRGB => Some(("bc1", 8)),
        DXGI_BC2 | DXGI_BC2_SRGB => Some(("bc2", 16)),
        DXGI_BC3 | DXGI_BC3_SRGB => Some(("bc3", 16)),
        DXGI_BC7 | DXGI_BC7_SRGB => Some(("bc7", 16)),
        _ => None,
    }
}

/// ([(w, h)], [byte size]) per mip, largest first, for a Format; a volume texture (depth > 1) has max(1, depth >> level) slices
/// per mip, all in the size.
pub fn mip_sizes(w: usize, h: usize, mips: usize, fi: &Format, depth: usize) -> (Vec<(usize, usize)>, Vec<usize>) {
    let dims: Vec<(usize, usize)> = (0..mips).map(|i| ((w >> i).max(1), (h >> i).max(1))).collect();
    let sizes = dims
        .iter()
        .enumerate()
        .map(|(i, &(mw, mh))| {
            let n = (depth >> i).max(1);
            if fi.block == 1 {
                mw * mh * fi.bytes * n
            } else {
                ((mw + 3) / 4).max(1) * ((mh + 3) / 4).max(1) * fi.bytes * n
            }
        })
        .collect();
    (dims, sizes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_header(flags: u32, bits: u32, masks: [u32; 4], caps2: u32) -> Vec<u8> {
        let mut d = vec![0u8; 128];
        d[..4].copy_from_slice(b"DDS ");
        for (o, v) in [(12, 4u32), (16, 4), (28, 1), (80, flags), (88, bits), (92, masks[0]), (96, masks[1]), (100, masks[2]), (104, masks[3]), (112, caps2)] {
            d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        d
    }

    #[test]
    fn legacy_pixel_formats_and_decode() {
        let h = |f, b, m| parse_header(&legacy_header(f, b, m, 0)).unwrap().dxgi;
        assert_eq!(h(0x41, 32, [0xff0000, 0xff00, 0xff, 0xff000000]), Some(87));
        assert_eq!(h(0x40, 24, [0xff0000, 0xff00, 0xff, 0]), Some(DXGI_PRIVATE_BGR8));
        assert_eq!(h(0x40, 16, [0xffff, 0, 0, 0]), Some(56));
        assert_eq!(h(0x40, 32, [0xffff, 0xffff0000, 0, 0]), Some(35));
        assert_eq!(h(0x2, 8, [0, 0, 0, 0xff]), Some(65));
        assert_eq!(h(0x20000, 8, [0xff, 0, 0, 0]), Some(61));
        assert_eq!(h(0x80000, 32, [0xffff, 0xffff0000, 0, 0]), None);
        // decoders (2 x 1 pixels)
        assert_eq!(decode("bgr8", &[1, 2, 3, 4, 5, 6], 2, 1).unwrap(), [3, 2, 1, 255, 6, 5, 4, 255]);
        assert_eq!(decode("bgrx8", &[1, 2, 3, 0, 4, 5, 6, 0], 2, 1).unwrap(), [3, 2, 1, 255, 6, 5, 4, 255]);
        assert_eq!(decode("rg8", &[10, 20, 30, 40], 2, 1).unwrap(), [10, 20, 0, 255, 30, 40, 0, 255]);
        assert_eq!(decode("r16", &[0x34, 0x12, 0xff, 0xff], 2, 1).unwrap(), [0x12, 0x12, 0x12, 255, 255, 255, 255, 255]);
        assert_eq!(decode("rg16", &[0, 1, 0, 2], 2, 1).unwrap()[..4], [1, 2, 0, 255]);
        assert_eq!(decode("a8", &[7, 9], 2, 1).unwrap(), [7, 7, 7, 255, 9, 9, 9, 255]);
        for n in ["a8", "rg8", "bgrx8", "r16", "rg16", "bgr8", "bc6h", "bc6hs"] {
            assert!(DECODABLE.contains(&n), "{}", n);
        }
        assert_eq!(format_info(49).unwrap().name, "rg8");
        assert_eq!(format_info(93).unwrap().name, "bgrx8");
        assert_eq!(format_info(95).unwrap().name, "bc6h");
        assert_eq!(format_info(96).unwrap().name, "bc6hs");
    }

    #[test]
    fn cube_map_faces() {
        let plain = legacy_header(0x40, 32, [0xff0000, 0xff00, 0xff, 0], 0);
        assert_eq!(parse_faces(&plain), 1);
        assert_eq!(parse_faces(&legacy_header(0x40, 32, [0xff0000, 0xff00, 0xff, 0], 0x200 | 0xFC00)), 6);
        assert_eq!(parse_faces(&legacy_header(0x40, 32, [0xff0000, 0xff00, 0xff, 0], 0x200 | 0x400 | 0x800)), 2);
        // DX10 header with the TEXTURECUBE misc flag
        let mut d = legacy_header(0x4, 0, [0; 4], 0);
        d[84..88].copy_from_slice(b"DX10");
        d.extend_from_slice(&[0u8; 20]);
        d[128..132].copy_from_slice(&95u32.to_le_bytes());
        d[132..136].copy_from_slice(&3u32.to_le_bytes());
        d[136..140].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(parse_faces(&d), 6);
    }

    #[test]
    fn bc6h_decodes_through_dds() {
        // one mode-11 block with every endpoint at its maximum (0x3ff unsigned, 0x1ff signed) = 65504: white after the tone map
        for (f, e) in [("bc6h", 0x3ffu128), ("bc6hs", 0x1ff)] {
            let v: u128 = 0b00011 | (0..6).fold(0u128, |a, i| a | (e << (5 + 10 * i)));
            let px = decode(f, &v.to_le_bytes(), 4, 4).unwrap();
            assert!(px.chunks_exact(4).all(|p| p == [255, 255, 255, 255]), "{}", f);
        }
        let blk = (0b00011u128 | (0..6).fold(0u128, |a, i| a | (0x3ffu128 << (5 + 10 * i)))).to_le_bytes();
        // a 2 x 2 image only uses the top-left corner of the block
        assert_eq!(decode("bc6h", &blk, 2, 2).unwrap().len(), 16);
    }

    #[test]
    fn bc1_two_colour_block() {
        // c0 = pure red (0xF800) > c1 = pure blue (0x001F): four-colour mode; indices all 0 -> red
        let blk = [0x00, 0xF8, 0x1F, 0x00, 0, 0, 0, 0];
        let px = decode_bc("bc1", &blk, 4, 4);
        assert_eq!(&px[..4], &[255, 0, 0, 255]);
        assert_eq!(px.len(), 64);
        // index 2 everywhere = (2*red + blue) / 3 -> (170, 0, 85)
        let blk2 = [0x00, 0xF8, 0x1F, 0x00, 0xAA, 0xAA, 0xAA, 0xAA];
        assert_eq!(&decode_bc("bc1", &blk2, 4, 4)[..4], &[170, 0, 85, 255]);
    }

    #[test]
    fn bc1_punch_through() {
        // c0 <= c1: three-colour mode, index 3 is transparent black
        let blk = [0x1F, 0x00, 0x00, 0xF8, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(&decode_bc("bc1", &blk, 4, 4)[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn bc3_alpha_palette() {
        let pal = bc4_palette(255, 0);
        assert_eq!(pal[2], 218); // (6*255)/7
        let pal6 = bc4_palette(0, 255);
        assert_eq!((pal6[6], pal6[7]), (0, 255));
    }

    #[test]
    fn partial_image_and_short_data() {
        // 5x5 image = 2x2 blocks; data for the first block only: the rest stays zero
        let blk = [0x00, 0xF8, 0x1F, 0x00, 0, 0, 0, 0];
        let px = decode_bc("bc1", &blk, 5, 5);
        assert_eq!(px.len(), 100);
        assert_eq!(&px[..4], &[255, 0, 0, 255]);
        assert_eq!(&px[4 * 4..4 * 4 + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn bc1_alpha_coverage_counts_punch_through() {
        // three-colour mode (c0 <= c1): index 3 texels are transparent; here 8 of the 16 texels
        let blk = [0x1F, 0x00, 0x00, 0xF8, 0xFF, 0xFF, 0x00, 0x00];
        assert_eq!(bc1_alpha_coverage(&blk, 4, 4, 0.5), Some(0.5));
        // four-colour mode (c0 > c1): everything is opaque
        let opaque = [0x00, 0xF8, 0x1F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(bc1_alpha_coverage(&opaque, 4, 4, 0.5), Some(1.0));
        // a 3 x 3 image counts only its 9 texels
        assert_eq!(bc1_alpha_coverage(&opaque, 3, 3, 0.5), Some(1.0));
        assert_eq!(bc1_alpha_coverage(&[], 4, 4, 0.5), None);
    }

    #[test]
    fn bc3_and_bc2_alpha_coverage() {
        // BC3: a0 = 255, a1 = 0, every index 0 -> alpha 255; every index 1 -> alpha 0
        let mut blk = [0u8; 16];
        blk[0] = 255;
        assert_eq!(bc2_bc3_alpha_coverage("bc3", &blk, 4, 4, 0.5), Some(1.0));
        blk[2..8].copy_from_slice(&[0x49, 0x92, 0x24, 0x49, 0x92, 0x24]); // 001 001 ...
        assert_eq!(bc2_bc3_alpha_coverage("bc3", &blk, 4, 4, 0.5), Some(0.0));
        // BC2: 4-bit alpha, 0xF -> 255; half of the texels
        let mut b2 = [0u8; 16];
        b2[..4].fill(0xFF);
        assert_eq!(bc2_bc3_alpha_coverage("bc2", &b2, 4, 4, 0.5), Some(0.5));
        // threshold: cut 1.0 needs alpha 255 (int(1.0 * 255 + 0.999) = 255)
        assert_eq!(bc2_bc3_alpha_coverage("bc2", &b2, 4, 4, 1.0), Some(0.5));
    }

    #[test]
    fn bc7_alpha_coverage_modes() {
        // mode 6 (bit 6 set): both 7-bit alpha endpoints 127 plus p-bits 1 -> 255; indices 0
        let mut v: u128 = 1 << 6;
        v |= 127u128 << 49;
        v |= 127u128 << 56;
        v |= 1u128 << 63;
        v |= 1u128 << 64;
        assert_eq!(bc7_alpha_coverage(&v.to_le_bytes(), 4, 4, 0.5), Some(1.0));
        let z: u128 = 1 << 6;
        assert_eq!(bc7_alpha_coverage(&z.to_le_bytes(), 4, 4, 0.5), Some(0.0));
        // modes 0-3 carry no alpha: opaque
        assert_eq!(bc7_alpha_coverage(&1u128.to_le_bytes(), 4, 4, 0.5), Some(1.0));
        // mode 7 with mixed endpoints counts half: alpha endpoints 0, 0, 31<<..: build 4 endpoints (5 bits) = 0,0,31,31
        let mut m7: u128 = 1 << 7;
        m7 |= 31u128 << (74 + 10);
        m7 |= 31u128 << (74 + 15);
        assert_eq!(bc7_alpha_coverage(&m7.to_le_bytes(), 4, 4, 0.5), Some(0.5));
    }

    #[test]
    fn mip_chain() {
        let fi = format_info(71).unwrap();
        let (dims, sizes) = mip_sizes(8, 8, 4, &fi, 1);
        assert_eq!(dims, vec![(8, 8), (4, 4), (2, 2), (1, 1)]);
        assert_eq!(sizes, vec![32, 8, 8, 8]);
    }
}

// ---- alpha coverage (used by the material registry's `acov`)

/// Smallest 8-bit alpha that is >= `cut` (`cut * 255 + 0.999`, truncated).
fn alpha_threshold(cut: f64) -> i64 {
    (cut * 255.0 + 0.999) as i64
}

fn coverage(hit: f64, total: u64) -> Option<f64> {
    if total > 0 {
        Some(hit / total as f64)
    } else {
        None
    }
}

/// Fraction of texels whose alpha is >= cut (0..1) of a BC2 / BC3 mip, reading only the 8-byte alpha half of each block (no colour
/// decode).  `fmt` is "bc2"; anything else is read as BC3.
///
/// Why: an alpha-tested texture that is a 0/1 mask blurs to its mean alpha on the far mips, where the plain test turns solid or
/// vanishes; the viewer keeps the mip-0 coverage instead.  Texels outside w x h (blocks past the edge) are
/// not counted.  EMPIRICAL (trelace_grate_diff, BC3): mean alpha 0.418 on every mip, share >= 0.33 is 0.418 at mip 0 and 0.81 at
/// mip 8.
pub fn bc2_bc3_alpha_coverage(fmt: &str, data: &[u8], w: usize, h: usize, cut: f64) -> Option<f64> {
    let (bw, bh) = (((w + 3) / 4).max(1), ((h + 3) / 4).max(1));
    let thr = alpha_threshold(cut);
    let (mut hit, mut total, mut pos) = (0u64, 0u64, 0usize);
    for by in 0..bh {
        for bx in 0..bw {
            let Some(blk) = data.get(pos..pos + 16) else { return coverage(hit as f64, total) };
            pos += 16;
            let alpha: [u8; 16] = if fmt == "bc2" {
                let a64 = u64::from_le_bytes(blk[..8].try_into().unwrap());
                std::array::from_fn(|i| (((a64 >> (4 * i)) & 15) * 17) as u8)
            } else {
                let pal = bc4_palette(blk[0], blk[1]);
                let mut ab = [0u8; 8];
                ab[..6].copy_from_slice(&blk[2..8]);
                let ab = u64::from_le_bytes(ab);
                std::array::from_fn(|i| pal[((ab >> (3 * i)) & 7) as usize])
            };
            for (i, &a) in alpha.iter().enumerate() {
                if bx * 4 + (i & 3) < w && by * 4 + (i >> 2) < h {
                    total += 1;
                    hit += (a as i64 >= thr) as u64;
                }
            }
        }
    }
    coverage(hit as f64, total)
}

/// Fraction of texels with alpha >= cut of a BC1 mip.  BC1 has 1-bit alpha: a block whose first colour is <= its second (as
/// little-endian 16-bit values) is in three-colour mode and its index-3 texels are transparent, every other texel is opaque (alpha
/// 255).  A browser that uploads BC1 as COMPRESSED_RGBA_S3TC_DXT1 reads the same bits, so this matches what the
/// shader tests.  CONFIRMED against the S3TC / D3D BC1 definition; EMPIRICAL on the ship cut-out textures.
pub fn bc1_alpha_coverage(data: &[u8], w: usize, h: usize, cut: f64) -> Option<f64> {
    let (bw, bh) = (((w + 3) / 4).max(1), ((h + 3) / 4).max(1));
    let thr = alpha_threshold(cut);
    let (mut hit, mut total, mut pos) = (0u64, 0u64, 0usize);
    for by in 0..bh {
        for bx in 0..bw {
            let Some(blk) = data.get(pos..pos + 8) else { return coverage(hit as f64, total) };
            pos += 8;
            let c0 = blk[0] as u32 | (blk[1] as u32) << 8;
            let c1 = blk[2] as u32 | (blk[3] as u32) << 8;
            let bits = u32::from_le_bytes(blk[4..8].try_into().unwrap());
            for i in 0..16 {
                if bx * 4 + (i & 3) < w && by * 4 + (i >> 2) < h {
                    total += 1;
                    hit += ((c0 > c1 || (bits >> (2 * i)) & 3 != 3) && thr <= 255) as u64;
                }
            }
        }
    }
    coverage(hit as f64, total)
}

const BC7_W2: [i64; 4] = [0, 21, 43, 64];
const BC7_W3: [i64; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const BC7_W4: [i64; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// The 16 texel indices of a BC7 index set starting at bit `pos`: `bits` wide, the first texel (the anchor of a one-subset mode)
/// one bit fewer.
fn bc7_indices(v: u128, mut pos: u32, bits: u32) -> [usize; 16] {
    let mut out = [0usize; 16];
    out[0] = ((v >> pos) & ((1 << (bits - 1)) - 1)) as usize;
    pos += bits - 1;
    for o in out.iter_mut().skip(1) {
        *o = ((v >> pos) & ((1 << bits) - 1)) as usize;
        pos += bits;
    }
    out
}

/// Fraction of texels with alpha >= cut of a BC7 mip (decodes only what the alpha needs; no colour output).
///
/// Modes 0-3 carry no alpha (255).  Modes 4, 5 and 6 are decoded exactly (CONFIRMED against the BPTC layout: mode 4 has 6-bit alpha
/// endpoints with a 3-bit or 2-bit index set chosen by idxMode, mode 5 8-bit endpoints with its own 2-bit set, mode 6 7-bit
/// endpoints plus p-bits with 4-bit indices; a channel rotation 1..3 swaps alpha with R / G / B, so the output alpha is then that
/// colour channel interpolated with the colour index set).  Mode 7 (two subsets, 4 alpha endpoints) is APPROXIMATED, because the
/// partition table that says which texel belongs to which subset (and where the second anchor, with a one bit shorter index, sits)
/// is not in this code base: a block whose four alpha endpoints are all >= cut counts open, all < cut closed, a mixed block counts
/// half.  In the ship cut-out textures mode 7 is ~7.5 % of the blocks.
pub fn bc7_alpha_coverage(data: &[u8], w: usize, h: usize, cut: f64) -> Option<f64> {
    let (bw, bh) = (((w + 3) / 4).max(1), ((h + 3) / 4).max(1));
    let thr = alpha_threshold(cut);
    let (mut hit, mut total, mut pos) = (0.0f64, 0u64, 0usize);
    for by in 0..bh {
        for bx in 0..bw {
            let Some(blk) = data.get(pos..pos + 16) else { return coverage(hit, total) };
            pos += 16;
            let v = u128::from_le_bytes(blk.try_into().unwrap());
            let b0 = blk[0];
            let mode = if b0 == 0 { 8 } else { b0.trailing_zeros() };
            let inside = |i: usize| bx * 4 + (i & 3) < w && by * 4 + (i >> 2) < h;
            let n = (0..16).filter(|&i| inside(i)).count();
            if !(4..=7).contains(&mode) {
                hit += if thr <= 255 { n as f64 } else { 0.0 };
            } else if mode == 7 {
                let al: Vec<i64> = (0..4)
                    .map(|k| {
                        let a = ((((v >> (74 + 5 * k)) & 31) << 1) | ((v >> (94 + k)) & 1)) as i64;
                        (a << 2) | (a >> 4)
                    })
                    .collect();
                let (mn, mx) = (*al.iter().min().unwrap(), *al.iter().max().unwrap());
                hit += n as f64 * if mn >= thr { 1.0 } else if mx < thr { 0.0 } else { 0.5 };
            } else {
                let (a0, a1, idx, wt): (i64, i64, [usize; 16], &[i64]);
                if mode == 6 {
                    a0 = ((((v >> 49) & 127) << 1) | ((v >> 63) & 1)) as i64;
                    a1 = ((((v >> 56) & 127) << 1) | ((v >> 64) & 1)) as i64;
                    idx = bc7_indices(v, 65, 4);
                    wt = &BC7_W4;
                } else {
                    // (rot, colour indices, colour weights, alpha indices, alpha weights, alpha endpoints, channel endpoint offsets, endpoint width)
                    let rot;
                    let (ci, cw, ai, aw): ([usize; 16], &[i64], [usize; 16], &[i64]);
                    let (e0, e1): (i64, i64);
                    let (offs, bits): ([u32; 6], u32);
                    if mode == 5 {
                        rot = ((v >> 6) & 3) as usize;
                        (e0, e1) = (((v >> 50) & 255) as i64, ((v >> 58) & 255) as i64);
                        (ci, cw, ai, aw) = (bc7_indices(v, 66, 2), &BC7_W2, bc7_indices(v, 97, 2), &BC7_W2);
                        (offs, bits) = ([8, 15, 22, 29, 36, 43], 7);
                    } else {
                        rot = ((v >> 5) & 3) as usize;
                        let (x0, x1) = (((v >> 38) & 63) as i64, ((v >> 44) & 63) as i64);
                        (e0, e1) = ((x0 << 2) | (x0 >> 4), (x1 << 2) | (x1 >> 4));
                        let (i2, i3) = (bc7_indices(v, 50, 2), bc7_indices(v, 81, 3));
                        if (v >> 7) & 1 == 1 {
                            (ci, cw, ai, aw) = (i3, &BC7_W3, i2, &BC7_W2);
                        } else {
                            (ci, cw, ai, aw) = (i2, &BC7_W2, i3, &BC7_W3);
                        }
                        (offs, bits) = ([8, 13, 18, 23, 28, 33], 5);
                    }
                    // the endpoint expansion of the colour channels: 7-bit (mode 5) x<<1|x>>6, 5-bit (mode 4) x<<3|x>>2
                    let e = |o: u32| {
                        let x = ((v >> o) & ((1 << bits) - 1)) as i64;
                        if bits == 7 {
                            (x << 1) | (x >> 6)
                        } else {
                            (x << 3) | (x >> 2)
                        }
                    };
                    if rot != 0 {
                        // alpha comes from colour channel rot-1 (R, G, B) with the colour indices
                        a0 = e(offs[2 * (rot - 1)]);
                        a1 = e(offs[2 * (rot - 1) + 1]);
                        idx = ci;
                        wt = cw;
                    } else {
                        a0 = e0;
                        a1 = e1;
                        idx = ai;
                        wt = aw;
                    }
                }
                for i in 0..16 {
                    if inside(i) {
                        let wgt = wt[idx[i]];
                        if ((a0 * (64 - wgt) + a1 * wgt + 32) >> 6) >= thr {
                            hit += 1.0;
                        }
                    }
                }
            }
            total += n as u64;
        }
    }
    coverage(hit, total)
}
