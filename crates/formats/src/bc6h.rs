//! BC6H (BPTC HDR, DXGI 95 unsigned / 96 signed float16) block decoder, plus the tone map the viewer uses to show it as 8-bit LDR.
//!
//! Layout and arithmetic follow the public BC6H description (Microsoft Direct3D 11 functional spec "BC6H Format", the same layout
//! EXT_texture_compression_bptc and the Khronos Data Format spec use) as implemented by the public `bcdec` decoder, which was read
//! (not compiled) as the reference for the 14 mode bit layouts.  A block is `mode` (2 or 5 bits), the endpoints in a scattered bit
//! layout (modes 1-10: two subsets `w x` and `y z`, 5 partition bits, 46 index bits; modes 11-14: one subset, 63 index bits), then
//! the indices (3 bit for two subsets, 4 bit for one; the first texel of each subset stores one bit fewer).  Endpoint w has the full
//! precision and, in the "transformed" modes (all except 10 and 11), the others are signed deltas from it.  Endpoints are unquantised
//! to 16 bits, interpolated with the BC7 3 / 4 bit weights, then scaled by 31/64 (unsigned) or 31/32 (signed magnitude) to the
//! half-float bit pattern.  Mode patterns 10011, 10111, 11011, 11111 are reserved and decode to black.
//!
//! The 2-subset shapes and anchors are the first 32 entries of the BC7 tables in `bc7` (the BPTC partition set is shared); the test
//! `partition_tables_match_bcdec` compares them with the 32 shapes bcdec lists, and `mode_bits_add_up` checks that every mode's
//! field list fills exactly 128 bits.  Beyond that the layouts are EMPIRICAL (transcribed from bcdec, no engine address): the
//! tests below use hand-built blocks, and p4k-scene decodes a real cubemap from the archive.
//!
//! Tone map (a viewer choice, not an engine fact): half floats are linear light, possibly > 1; `tonemap` clamps negatives to 0, keeps
//! the range up to `KNEE` linear, rolls the rest off to 1 with a Reinhard-style shoulder, and encodes the result with the sRGB curve
//! so the 8-bit PNG looks like the LDR textures next to it.

use crate::bc7::{ANCHOR2, PART2};
use std::sync::OnceLock;

/// One endpoint bit-field: (field, bit count, shift, reversed).  Fields are `channel * 4 + endpoint` (w x y z = 0..3; r = 0, g = 4, b = 8).
type Op = (u8, u8, u8, bool);
const R: u8 = 0;
const G: u8 = 4;
const B: u8 = 8;

const fn o(f: u8, n: u8, s: u8) -> Op {
    (f, n, s, false)
}
const fn rv(f: u8, n: u8, s: u8) -> Op {
    (f, n, s, true)
}

struct Mode {
    /// pattern read from the low bits (2 or 5 bits)
    pattern: u8,
    pattern_bits: u8,
    /// precision of the base endpoint and of the r / g / b deltas (or of the endpoints themselves when not transformed)
    wbits: i32,
    dbits: [i32; 3],
    transformed: bool,
    two_subsets: bool,
    ops: &'static [Op],
}

#[rustfmt::skip]
const MODES: [Mode; 14] = [
    Mode { pattern: 0b00, pattern_bits: 2, wbits: 10, dbits: [5, 5, 5], transformed: true, two_subsets: true, ops: &[
        o(G + 2, 1, 4), o(B + 2, 1, 4), o(B + 3, 1, 4), o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 5, 0), o(G + 3, 1, 4), o(G + 2, 4, 0),
        o(G + 1, 5, 0), o(B + 3, 1, 0), o(G + 3, 4, 0), o(B + 1, 5, 0), o(B + 3, 1, 1), o(B + 2, 4, 0), o(R + 2, 5, 0), o(B + 3, 1, 2),
        o(R + 3, 5, 0), o(B + 3, 1, 3)] },
    Mode { pattern: 0b01, pattern_bits: 2, wbits: 7, dbits: [6, 6, 6], transformed: true, two_subsets: true, ops: &[
        o(G + 2, 1, 5), o(G + 3, 1, 4), o(G + 3, 1, 5), o(R, 7, 0), o(B + 3, 1, 0), o(B + 3, 1, 1), o(B + 2, 1, 4), o(G, 7, 0), o(B + 2, 1, 5),
        o(B + 3, 1, 2), o(G + 2, 1, 4), o(B, 7, 0), o(B + 3, 1, 3), o(B + 3, 1, 5), o(B + 3, 1, 4), o(R + 1, 6, 0), o(G + 2, 4, 0),
        o(G + 1, 6, 0), o(G + 3, 4, 0), o(B + 1, 6, 0), o(B + 2, 4, 0), o(R + 2, 6, 0), o(R + 3, 6, 0)] },
    Mode { pattern: 0b00010, pattern_bits: 5, wbits: 11, dbits: [5, 4, 4], transformed: true, two_subsets: true, ops: &[
        o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 5, 0), o(R, 1, 10), o(G + 2, 4, 0), o(G + 1, 4, 0), o(G, 1, 10), o(B + 3, 1, 0),
        o(G + 3, 4, 0), o(B + 1, 4, 0), o(B, 1, 10), o(B + 3, 1, 1), o(B + 2, 4, 0), o(R + 2, 5, 0), o(B + 3, 1, 2), o(R + 3, 5, 0),
        o(B + 3, 1, 3)] },
    Mode { pattern: 0b00110, pattern_bits: 5, wbits: 11, dbits: [4, 5, 4], transformed: true, two_subsets: true, ops: &[
        o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 4, 0), o(R, 1, 10), o(G + 3, 1, 4), o(G + 2, 4, 0), o(G + 1, 5, 0), o(G, 1, 10),
        o(G + 3, 4, 0), o(B + 1, 4, 0), o(B, 1, 10), o(B + 3, 1, 1), o(B + 2, 4, 0), o(R + 2, 4, 0), o(B + 3, 1, 0), o(B + 3, 1, 2),
        o(R + 3, 4, 0), o(G + 2, 1, 4), o(B + 3, 1, 3)] },
    Mode { pattern: 0b01010, pattern_bits: 5, wbits: 11, dbits: [4, 4, 5], transformed: true, two_subsets: true, ops: &[
        o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 4, 0), o(R, 1, 10), o(B + 2, 1, 4), o(G + 2, 4, 0), o(G + 1, 4, 0), o(G, 1, 10),
        o(B + 3, 1, 0), o(G + 3, 4, 0), o(B + 1, 5, 0), o(B, 1, 10), o(B + 2, 4, 0), o(R + 2, 4, 0), o(B + 3, 1, 1), o(B + 3, 1, 2),
        o(R + 3, 4, 0), o(B + 3, 1, 4), o(B + 3, 1, 3)] },
    Mode { pattern: 0b01110, pattern_bits: 5, wbits: 9, dbits: [5, 5, 5], transformed: true, two_subsets: true, ops: &[
        o(R, 9, 0), o(B + 2, 1, 4), o(G, 9, 0), o(G + 2, 1, 4), o(B, 9, 0), o(B + 3, 1, 4), o(R + 1, 5, 0), o(G + 3, 1, 4), o(G + 2, 4, 0),
        o(G + 1, 5, 0), o(B + 3, 1, 0), o(G + 3, 4, 0), o(B + 1, 5, 0), o(B + 3, 1, 1), o(B + 2, 4, 0), o(R + 2, 5, 0), o(B + 3, 1, 2),
        o(R + 3, 5, 0), o(B + 3, 1, 3)] },
    Mode { pattern: 0b10010, pattern_bits: 5, wbits: 8, dbits: [6, 5, 5], transformed: true, two_subsets: true, ops: &[
        o(R, 8, 0), o(G + 3, 1, 4), o(B + 2, 1, 4), o(G, 8, 0), o(B + 3, 1, 2), o(G + 2, 1, 4), o(B, 8, 0), o(B + 3, 1, 3), o(B + 3, 1, 4),
        o(R + 1, 6, 0), o(G + 2, 4, 0), o(G + 1, 5, 0), o(B + 3, 1, 0), o(G + 3, 4, 0), o(B + 1, 5, 0), o(B + 3, 1, 1), o(B + 2, 4, 0),
        o(R + 2, 6, 0), o(R + 3, 6, 0)] },
    Mode { pattern: 0b10110, pattern_bits: 5, wbits: 8, dbits: [5, 6, 5], transformed: true, two_subsets: true, ops: &[
        o(R, 8, 0), o(B + 3, 1, 0), o(B + 2, 1, 4), o(G, 8, 0), o(G + 2, 1, 5), o(G + 2, 1, 4), o(B, 8, 0), o(G + 3, 1, 5), o(B + 3, 1, 4),
        o(R + 1, 5, 0), o(G + 3, 1, 4), o(G + 2, 4, 0), o(G + 1, 6, 0), o(G + 3, 4, 0), o(B + 1, 5, 0), o(B + 3, 1, 1), o(B + 2, 4, 0),
        o(R + 2, 5, 0), o(B + 3, 1, 2), o(R + 3, 5, 0), o(B + 3, 1, 3)] },
    Mode { pattern: 0b11010, pattern_bits: 5, wbits: 8, dbits: [5, 5, 6], transformed: true, two_subsets: true, ops: &[
        o(R, 8, 0), o(B + 3, 1, 1), o(B + 2, 1, 4), o(G, 8, 0), o(B + 2, 1, 5), o(G + 2, 1, 4), o(B, 8, 0), o(B + 3, 1, 5), o(B + 3, 1, 4),
        o(R + 1, 5, 0), o(G + 3, 1, 4), o(G + 2, 4, 0), o(G + 1, 5, 0), o(B + 3, 1, 0), o(G + 3, 4, 0), o(B + 1, 6, 0), o(B + 2, 4, 0),
        o(R + 2, 5, 0), o(B + 3, 1, 2), o(R + 3, 5, 0), o(B + 3, 1, 3)] },
    Mode { pattern: 0b11110, pattern_bits: 5, wbits: 6, dbits: [6, 6, 6], transformed: false, two_subsets: true, ops: &[
        o(R, 6, 0), o(G + 3, 1, 4), o(B + 3, 1, 0), o(B + 3, 1, 1), o(B + 2, 1, 4), o(G, 6, 0), o(G + 2, 1, 5), o(B + 2, 1, 5), o(B + 3, 1, 2),
        o(G + 2, 1, 4), o(B, 6, 0), o(G + 3, 1, 5), o(B + 3, 1, 3), o(B + 3, 1, 5), o(B + 3, 1, 4), o(R + 1, 6, 0), o(G + 2, 4, 0),
        o(G + 1, 6, 0), o(G + 3, 4, 0), o(B + 1, 6, 0), o(B + 2, 4, 0), o(R + 2, 6, 0), o(R + 3, 6, 0)] },
    Mode { pattern: 0b00011, pattern_bits: 5, wbits: 10, dbits: [10, 10, 10], transformed: false, two_subsets: false, ops: &[
        o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 10, 0), o(G + 1, 10, 0), o(B + 1, 10, 0)] },
    Mode { pattern: 0b00111, pattern_bits: 5, wbits: 11, dbits: [9, 9, 9], transformed: true, two_subsets: false, ops: &[
        o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 9, 0), o(R, 1, 10), o(G + 1, 9, 0), o(G, 1, 10), o(B + 1, 9, 0), o(B, 1, 10)] },
    Mode { pattern: 0b01011, pattern_bits: 5, wbits: 12, dbits: [8, 8, 8], transformed: true, two_subsets: false, ops: &[
        o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 8, 0), rv(R, 2, 10), o(G + 1, 8, 0), rv(G, 2, 10), o(B + 1, 8, 0), rv(B, 2, 10)] },
    Mode { pattern: 0b01111, pattern_bits: 5, wbits: 16, dbits: [4, 4, 4], transformed: true, two_subsets: false, ops: &[
        o(R, 10, 0), o(G, 10, 0), o(B, 10, 0), o(R + 1, 4, 0), rv(R, 6, 10), o(G + 1, 4, 0), rv(G, 6, 10), o(B + 1, 4, 0), rv(B, 6, 10)] },
];

const W3: [i32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const W4: [i32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

fn extend_sign(v: i32, bits: i32) -> i32 {
    (v << (32 - bits)) >> (32 - bits)
}

fn unquantize(val: i32, bits: i32, signed: bool) -> i32 {
    if !signed {
        if bits >= 15 {
            val
        } else if val == 0 {
            0
        } else if val == (1 << bits) - 1 {
            0xFFFF
        } else {
            ((val << 16) + 0x8000) >> bits
        }
    } else if bits >= 16 {
        val
    } else {
        let (neg, v) = if val < 0 { (true, -val) } else { (false, val) };
        let u = if v == 0 {
            0
        } else if v >= (1 << (bits - 1)) - 1 {
            0x7FFF
        } else {
            ((v << 15) + 0x4000) >> (bits - 1)
        };
        if neg {
            -u
        } else {
            u
        }
    }
}

/// Final scale to the half-float bit pattern (sign-magnitude for the signed format).
fn finish(val: i32, signed: bool) -> u16 {
    if !signed {
        ((val * 31) >> 6) as u16
    } else {
        let v = if val < 0 { -(((-val) * 31) >> 5) } else { (val * 31) >> 5 };
        if v < 0 {
            0x8000 | (-v) as u16
        } else {
            v as u16
        }
    }
}

/// Mode index (0..14) of a block, None for the reserved patterns.
pub fn block_mode(blk: &[u8]) -> Option<usize> {
    let b = blk[0];
    MODES.iter().position(|m| b & ((1u8 << m.pattern_bits) - 1) == m.pattern)
}

/// Decodes one 16-byte block to 16 texels (row-major 4x4) of half-float bit patterns, R G B.  Reserved modes give zeros.
pub fn decode_block_half(blk: &[u8], signed: bool, out: &mut [[u16; 3]; 16]) {
    decode_block_half_flag(blk, signed, out);
}

/// `decode_block_half`; returns true when a signed block has a negative endpoint (used by the Pillow comparison test, see there).
fn decode_block_half_flag(blk: &[u8], signed: bool, out: &mut [[u16; 3]; 16]) -> bool {
    let mut wrapped = false;
    let Some(mode) = block_mode(blk) else {
        *out = [[0; 3]; 16];
        return false;
    };
    let m = &MODES[mode];
    let v = u128::from_le_bytes(blk[..16].try_into().unwrap());
    let mut pos = m.pattern_bits as u32;
    let mut rd = |n: u32| -> i32 {
        let x = ((v >> pos) & ((1u128 << n) - 1)) as i32;
        pos += n;
        x
    };
    let mut f = [0i32; 12];
    for &(fld, n, shift, rev) in m.ops {
        let mut x = rd(n as u32);
        if rev {
            x = (0..n).fold(0, |a, i| (a << 1) | ((x >> i) & 1));
        }
        f[fld as usize] |= x << shift;
    }
    let partition = if m.two_subsets { rd(5) as usize } else { 0 };
    let nep = if m.two_subsets { 4 } else { 2 };
    if signed {
        for c in 0..3 {
            f[c * 4] = extend_sign(f[c * 4], m.wbits);
        }
    }
    if m.transformed || signed {
        for c in 0..3 {
            for e in 1..nep {
                f[c * 4 + e] = extend_sign(f[c * 4 + e], m.dbits[c]);
            }
        }
    }
    if m.transformed {
        for c in 0..3 {
            let base = f[c * 4];
            for e in 1..nep {
                let mut x = (f[c * 4 + e] + base) & ((1 << m.wbits) - 1);
                if signed {
                    x = extend_sign(x, m.wbits);
                }
                f[c * 4 + e] = x;
            }
        }
    }
    wrapped |= signed && (0..3).any(|c| f[c * 4..c * 4 + nep].iter().any(|&x| x < 0));
    for c in 0..3 {
        for e in 0..nep {
            f[c * 4 + e] = unquantize(f[c * 4 + e], m.wbits, signed);
        }
    }
    let (weights, ibits): (&[i32], u32) = if m.two_subsets { (&W3, 3) } else { (&W4, 4) };
    for i in 0..16 {
        let (sub, anchor) = if m.two_subsets {
            ((PART2[partition] >> i) & 1, i == 0 || i == ANCHOR2[partition] as usize)
        } else {
            (0, i == 0)
        };
        let w = weights[rd(ibits - anchor as u32) as usize];
        let e = sub as usize * 2;
        for c in 0..3 {
            let (a, b) = (f[c * 4 + e], f[c * 4 + e + 1]);
            out[i][c] = finish((a * (64 - w) + b * w + 32) >> 6, signed);
        }
    }
    wrapped
}

/// Half-float bit pattern to f32 (Inf maps to +-65504, NaN to 0: this is for display).
pub fn half_to_f32(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let e = ((h >> 10) & 31) as i32;
    let m = (h & 0x3ff) as f32;
    match e {
        0 => s * m * (1.0 / 16777216.0),
        31 => {
            if m == 0.0 {
                s * 65504.0
            } else {
                0.0
            }
        }
        _ => s * (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Linear values up to this are shown unchanged (before the sRGB curve).
pub const KNEE: f32 = 0.8;

/// Linear HDR value -> 8-bit display value: negatives to 0, linear up to `KNEE`, above it a Reinhard-style shoulder
/// `KNEE + (1 - KNEE) * t / (1 + t)` with `t = (x - KNEE) / (1 - KNEE)` mapping [KNEE, inf) onto [KNEE, 1), then the sRGB curve.
pub fn tonemap(x: f32) -> u8 {
    let x = if x.is_nan() { 0.0 } else { x.max(0.0) };
    let l = if x <= KNEE {
        x
    } else {
        let t = (x - KNEE) / (1.0 - KNEE);
        KNEE + (1.0 - KNEE) * t / (1.0 + t)
    };
    let s = if l <= 0.0031308 { 12.92 * l } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

fn lut() -> &'static [u8] {
    static L: OnceLock<Vec<u8>> = OnceLock::new();
    L.get_or_init(|| (0..65536u32).map(|h| tonemap(half_to_f32(h as u16))).collect())
}

/// Tone-mapped RGBA8 of one block (alpha 255), for `dds::decode_blocks`.
pub fn decode_block_ldr(blk: &[u8], signed: bool, out: &mut [[u8; 4]; 16]) {
    let mut h = [[0u16; 3]; 16];
    decode_block_half(blk, signed, &mut h);
    let l = lut();
    for i in 0..16 {
        out[i] = [l[h[i][0] as usize], l[h[i][1] as usize], l[h[i][2] as usize], 255];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes fields LSB-first into a 128-bit block.
    struct W(u128, u32);
    impl W {
        fn put(&mut self, v: u128, n: u32) {
            self.0 |= (v & ((1u128 << n) - 1)) << self.1;
            self.1 += n;
        }
    }

    fn half(blk: u128, signed: bool) -> [[u16; 3]; 16] {
        let mut o = [[0u16; 3]; 16];
        decode_block_half(&blk.to_le_bytes(), signed, &mut o);
        o
    }

    /// A one-subset block: pattern, six (value, bits) endpoint fields given by the caller, then `idx` for texel 0 (3 bits) and `rest` for
    /// the others (4 bits each).
    fn one_subset(pattern: u128, fields: &[(u128, u32)], rest: u128) -> u128 {
        let mut w = W(0, 0);
        w.put(pattern, 5);
        for &(v, n) in fields {
            w.put(v, n);
        }
        w.put(0, 3);
        for _ in 1..16 {
            w.put(rest, 4);
        }
        w.0
    }

    #[test]
    fn partition_tables_match_bcdec() {
        // the 32 two-subset shapes of bcdec's `partition_sets`, as masks (bit i = texel i is in subset 1) and the fix-up texel of subset 1
        let shapes: [(u16, u8); 32] = [
            (0xcccc, 15), (0x8888, 15), (0xeeee, 15), (0xecc8, 15), (0xc880, 15), (0xfeec, 15), (0xfec8, 15), (0xec80, 15), (0xc800, 15),
            (0xffec, 15), (0xfe80, 15), (0xe800, 15), (0xffe8, 15), (0xff00, 15), (0xfff0, 15), (0xf000, 15), (0xf710, 15), (0x008e, 2),
            (0x7100, 8), (0x08ce, 2), (0x008c, 2), (0x7310, 8), (0x3100, 8), (0x8cce, 15), (0x088c, 2), (0x3110, 8), (0x6666, 2), (0x366c, 2),
            (0x17e8, 8), (0x0ff0, 8), (0x718e, 2), (0x399c, 2),
        ];
        for (p, &(mask, anchor)) in shapes.iter().enumerate() {
            assert_eq!(PART2[p], mask, "shape {}", p);
            assert_eq!(ANCHOR2[p], anchor, "anchor {}", p);
        }
    }

    #[test]
    fn mode_bits_add_up() {
        for (i, m) in MODES.iter().enumerate() {
            let ep: u32 = m.ops.iter().map(|o| o.1 as u32).sum();
            let (part, idx) = if m.two_subsets { (5, 46) } else { (0, 63) };
            assert_eq!(m.pattern_bits as u32 + ep + part + idx, 128, "mode {}", i + 1);
            // every bit of every field is written exactly once: field widths per endpoint = base bits (w) or delta bits
            let mut have = [0u32; 12];
            for &(f, n, s, _) in m.ops {
                have[f as usize] += (n as u32) << 0;
                assert!(s as u32 + n as u32 <= 16);
            }
            for c in 0..3 {
                assert_eq!(have[c * 4] as i32, m.wbits, "mode {} w", i + 1);
                for e in 1..if m.two_subsets { 4 } else { 2 } {
                    assert_eq!(have[c * 4 + e] as i32, m.dbits[c], "mode {} c{} e{}", i + 1, c, e);
                }
            }
        }
        for b in [0b10011u8, 0b10111, 0b11011, 0b11111] {
            assert!(block_mode(&[b; 16]).is_none());
        }
        for (i, m) in MODES.iter().enumerate() {
            assert_eq!(block_mode(&[m.pattern; 16]), Some(i), "pattern {:b}", m.pattern);
        }
        // reserved mode decodes to black
        assert!(half(0b10011, false).iter().all(|t| *t == [0; 3]));
    }

    #[test]
    fn mode11_solid_unsigned() {
        // mode 11: both endpoints 0x3FF -> 0xFFFF -> (0xFFFF * 31) >> 6 = 0x7BFF (65504, the largest finite half)
        let f = [(0x3ff, 10); 6];
        assert!(half(one_subset(0b00011, &f, 0), false).iter().all(|t| *t == [0x7bff; 3]));
        // endpoint 0x1ff: unquantise ((0x1ff << 16) + 0x8000) >> 10 = 0x7FE0
        let f = [(0x1ff, 10); 6];
        let want = ((((0x1ffi32 << 16) + 0x8000) >> 10) * 31 >> 6) as u16;
        assert!(half(one_subset(0b00011, &f, 0), false).iter().all(|t| *t == [want; 3]));
    }

    #[test]
    fn mode11_interpolates_between_endpoints() {
        // endpoint 0 = 0, endpoint 1 = 0x3FF; non-anchor indices 15 -> weight 64 -> endpoint 1, texel 0 (index 0) -> endpoint 0
        let f = [(0, 10), (0, 10), (0, 10), (0x3ff, 10), (0x3ff, 10), (0x3ff, 10)];
        let o = half(one_subset(0b00011, &f, 15), false);
        assert_eq!(o[0], [0; 3]);
        assert!(o[1..].iter().all(|t| *t == [0x7bff; 3]));
        // index 8 -> weight 34: ((0xFFFF * 34 + 32) >> 6) * 31 >> 6
        let o = half(one_subset(0b00011, &f, 8), false);
        assert_eq!(o[1], [(((0xffffi32 * 34 + 32) >> 6) * 31 >> 6) as u16; 3]);
    }

    #[test]
    fn mode11_signed_values() {
        // endpoint 0x200 = -512 (10 bit signed) -> |v| >= 511 -> 0x7FFF -> negated, finish: -(0x7FFF * 31 >> 5) = -31743 -> 0x8000 | 0x7BFF
        let o = half(one_subset(0b00011, &[(0x200, 10); 6], 0), true);
        assert!(o.iter().all(|t| *t == [0xfbff; 3]), "{:x?}", o[0]);
        assert_eq!(half_to_f32(0xfbff), -65504.0);
        let o = half(one_subset(0b00011, &[(0x1ff, 10); 6], 0), true);
        assert!(o.iter().all(|t| *t == [0x7bff; 3]));
        // a middle value: 0x100 = 256: ((256 << 15) + 0x4000) >> 9 = 16416, * 31 >> 5 = 15902
        let o = half(one_subset(0b00011, &[(0x100, 10); 6], 0), true);
        assert!(o.iter().all(|t| *t == [(16416 * 31 >> 5) as u16; 3]));
    }

    #[test]
    fn mode12_delta_transform() {
        // mode 12 (11.9): rw[9:0] gw[9:0] bw[9:0] rx[8:0] rw[10] gx[8:0] gw[10] bx[8:0] bw[10]: base 0x400 (bit 10 only), delta -1 (0x1ff)
        // -> endpoint 1 = 0x3ff
        let f = [(0, 10), (0, 10), (0, 10), (0x1ff, 9), (1, 1), (0x1ff, 9), (1, 1), (0x1ff, 9), (1, 1)];
        let o = half(one_subset(0b00111, &f, 15), false);
        assert_eq!(o[0], [finish(unquantize(0x400, 11, false), false); 3]);
        assert_eq!(o[1], [finish(unquantize(0x3ff, 11, false), false); 3]);
        // signed: base 0x3ff + 0 is sign-extended from 11 bits (0x3ff is +1023); delta 0x100 is -256 (9 bit)
        let f = [(0x3ff, 10), (0x3ff, 10), (0x3ff, 10), (0x100, 9), (0, 1), (0x100, 9), (0, 1), (0x100, 9), (0, 1)];
        let o = half(one_subset(0b00111, &f, 15), true);
        assert_eq!(o[1], [finish(unquantize(1023 - 256, 11, true), true); 3]);
    }

    #[test]
    fn mode13_and_14_reversed_high_bits() {
        // mode 13 (12.8): after each 8-bit delta come the two high base bits stored bit 11 first.  Stored "1, 0" -> base 0x800.
        let f = [(0, 10), (0, 10), (0, 10), (0, 8), (0b01, 2), (0, 8), (0b01, 2), (0, 8), (0b01, 2)];
        // put() is LSB first, so the value 0b01 in 2 bits writes a 1 first (the bit 11), then a 0
        let want = finish(unquantize(0x800, 12, false), false);
        assert!(half(one_subset(0b01011, &f, 0), false).iter().all(|t| *t == [want; 3]));
        // mode 14 (16.4): six stored bits, first one is bit 15 -> base 0x8000, used as 16 bit values as they are
        let f = [(0, 10), (0, 10), (0, 10), (0, 4), (0b000001, 6), (0, 4), (0b000001, 6), (0, 4), (0b000001, 6)];
        assert!(half(one_subset(0b01111, &f, 0), false).iter().all(|t| *t == [(0x8000 * 31 >> 6) as u16; 3]));
    }

    #[test]
    fn every_two_subset_mode_reads_its_fields() {
        // All fields zero except the first red base bits set; partition 1 puts texels 2, 3 ... in subset 1 (mask 0x8888: texel 3 of each row).
        // Texel 0 is in subset 0 with weight 0, so it shows endpoint w (red only); a texel of subset 1 shows endpoint y, which for the
        // transformed modes is w + delta(=0) = w, for the plain mode 10 is 0.
        for (mi, m) in MODES.iter().enumerate().filter(|(_, m)| m.two_subsets) {
            let mut w = W(0, 0);
            w.put(m.pattern as u128, m.pattern_bits as u32);
            let mut r_base = None;
            for &(f, n, _, _) in m.ops {
                if f == R && r_base.is_none() {
                    r_base = Some((w.1, n as u32));
                }
                w.put(0, n as u32);
            }
            w.put(1, 5); // partition 1: 0x8888 -> texel 3 (and 7, 11, 15) in subset 1
            let (p, n) = r_base.unwrap();
            let blk = w.0 | (((1u128 << n) - 1) << p);
            let o = half(blk, false);
            assert!(o[0][0] > 0 && o[0][1] == 0 && o[0][2] == 0, "mode {} {:x?}", mi + 1, o[0]);
            // subset 1 (texel 3, index 0 -> endpoint y): equal to w for transformed modes, black for mode 10
            if m.transformed {
                // delta 0 -> endpoint y equals w
                assert_eq!(o[3], o[0], "mode {}", mi + 1);
            } else {
                assert_eq!(o[3], [0; 3], "mode {}", mi + 1);
            }
        }
    }

    /// Cross-check with an independent decoder (Pillow 12.3 `BcnDecode`, which outputs `floor(clamp(value, 0, 1) * 255)`, EMPIRICAL from
    /// solid blocks): `P4K_BC6H_REF=<dir with blocks.bin (2000 random blocks per mode, mode bits forced), pil_u.bin, pil_s.bin>`.
    /// Skips without the directory.  Compares every texel; values over 1 or under 0 saturate in both, which hides little because about
    /// half of all random 16-bit endpoint values lie in [0, 1).  Unsigned (DXGI 95): all 28,000 blocks of the 14 modes agree to within
    /// one level (in fact exactly).  Signed (DXGI 96): blocks whose endpoints are all >= 0 agree in every mode; blocks with a negative
    /// endpoint are skipped because Pillow disagrees with the D3D / bcdec arithmetic implemented here for a minority of their texels
    /// (tiny positive results next to a negative endpoint come out as 255 in Pillow, only in the transformed modes).  Those texels are
    /// UNVERIFIED against a second decoder; the arithmetic is the spec's (sign-extend, transform with wrap, unquantise sign-magnitude).
    #[test]
    fn matches_pillow_on_random_blocks() {
        let Ok(dir) = std::env::var("P4K_BC6H_REF") else { return };
        let blocks = std::fs::read(format!("{}/blocks.bin", dir)).unwrap();
        for (signed, name) in [(false, "pil_u.bin"), (true, "pil_s.bin")] {
            let want = std::fs::read(format!("{}/{}", dir, name)).unwrap();
            let mut bad = [0usize; 14];
            let mut informative = [0usize; 14];
            let mut skipped = 0;
            for (bi, blk) in blocks.chunks_exact(16).enumerate() {
                let mut h = [[0u16; 3]; 16];
                let wrapped = decode_block_half_flag(blk, signed, &mut h);
                let mode = block_mode(blk).unwrap();
                if wrapped {
                    skipped += 1;
                    continue;
                }
                for t in 0..16 {
                    for c in 0..3 {
                        if signed && h[t][c] & 0x8000 != 0 {
                            continue;
                        }
                        let f = half_to_f32(h[t][c]).clamp(0.0, 1.0);
                        let got = (f * 255.0) as u8;
                        let w = want[(bi * 16 + t) * 3 + c];
                        if w > 0 && w < 255 {
                            informative[mode] += 1;
                        }
                        // Pillow's float -> 8 bit step differs from a plain truncation by one level at some boundaries
                        if (got as i32 - w as i32).abs() > 1 {
                            bad[mode] += 1;
                            if bad[mode] < 3 {
                                eprintln!("mode {} blk {} texel {} c {}: ours {} (half {:04x}) pillow {}", mode + 1, bi, t, c, got, h[t][c], w);
                            }
                        }
                    }
                }
            }
            eprintln!("skipped (wrapped) {}", skipped);
            eprintln!("signed={} mismatching channel values per mode: {:?}, informative: {:?}", signed, bad, informative);
            assert!(bad.iter().all(|&b| b == 0), "signed={} {:?}", signed, bad);
        }
    }

    #[test]
    fn tonemap_properties() {
        assert_eq!(tonemap(0.0), 0);
        assert_eq!(tonemap(-3.0), 0);
        assert_eq!(tonemap(f32::NAN), 0);
        assert_eq!(tonemap(1.0e6), 255);
        // linear below the knee: 0.5 linear is sRGB 188
        assert_eq!(tonemap(0.5), 188);
        let mut prev = 0;
        for i in 0..2000 {
            let t = tonemap(i as f32 * 0.01);
            assert!(t >= prev);
            prev = t;
        }
        // 1.0 does not clip (shoulder), brighter values are brighter
        assert!(tonemap(1.0) < 255 && tonemap(1.0) > tonemap(0.8));
        assert!(tonemap(8.0) > tonemap(1.0));
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
        // ldr block: all-0x7bff -> 255, all-zero -> 0
        let mut px = [[0u8; 4]; 16];
        decode_block_ldr(&one_subset(0b00011, &[(0x3ff, 10); 6], 0).to_le_bytes(), false, &mut px);
        assert_eq!(px[5], [255, 255, 255, 255]);
        decode_block_ldr(&[0b00011, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], false, &mut px);
        assert_eq!(px[5], [0, 0, 0, 255]);
    }
}
