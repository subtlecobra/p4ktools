//! BC7 (BPTC, DXGI 98/99) block decoder: all 8 modes to RGBA8.
//!
//! Layout (public BPTC / Khronos Data Format spec, section on BC7; the same layout the browser's EXT_texture_compression_bptc and
//! D3D11 use): the mode is the number of trailing zero bits of byte 0 (mode k = k zeros then a one; no one in the first byte is a
//! reserved block that decodes to transparent black).  Then, in order: partition (modes 0-3, 7), rotation (4, 5), index selector
//! (4), all R endpoints, all G, all B, all A (4-7), p-bits, then the index sets.  The first texel of each subset (its "anchor")
//! stores one bit fewer, its top bit being 0.
//!
//! Endpoint unquantisation: the p-bit (shared per subset in mode 1, per endpoint otherwise) is appended as the LSB, then the value
//! is expanded to 8 bits by replicating its top bits.  Colours interpolate with the weights 2 bit {0,21,43,64},
//! 3 bit {0,9,18,27,37,46,55,64}, 4 bit {0,4,9,...,64}: `(e0 * (64 - w) + e1 * w + 32) >> 6`.  Rotation (modes 4, 5) swaps A with
//! R, G or B after interpolation.  Modes 0-3 have no alpha (255).  sRGB (DXGI 99) is the same bits: like BC1-3 the stored values
//! are output as they are.
//!
//! EMPIRICAL: the 2- and 3-subset partition shapes and the anchor texel tables below were extracted from an independent decoder
//! (Pillow 12.3 `BcnDecode`) by decoding one synthetic block per partition (endpoints black/white, so the subset of every texel
//! and the texel that lost its MSB show up in the output); the anchor tables agree with the published ones.  The whole decoder was
//! also compared texel for texel with that decoder on random blocks of every mode.  The unit tests
//! below use hand-built blocks with hand-computed texels.

pub const PART2: [u16; 64] = [
    0xcccc, 0x8888, 0xeeee, 0xecc8, 0xc880, 0xfeec, 0xfec8, 0xec80, 0xc800, 0xffec, 0xfe80, 0xe800, 0xffe8, 0xff00, 0xfff0, 0xf000,
    0xf710, 0x008e, 0x7100, 0x08ce, 0x008c, 0x7310, 0x3100, 0x8cce, 0x088c, 0x3110, 0x6666, 0x366c, 0x17e8, 0x0ff0, 0x718e, 0x399c,
    0xaaaa, 0xf0f0, 0x5a5a, 0x33cc, 0x3c3c, 0x55aa, 0x9696, 0xa55a, 0x73ce, 0x13c8, 0x324c, 0x3bdc, 0x6996, 0xc33c, 0x9966, 0x0660,
    0x0272, 0x04e4, 0x4e40, 0x2720, 0xc936, 0x936c, 0x39c6, 0x639c, 0x9336, 0x9cc6, 0x817e, 0xe718, 0xccf0, 0x0fcc, 0x7744, 0xee22,
];
pub const PART3: [u32; 64] = [
    0xaa685050, 0x6a5a5040, 0x5a5a4200, 0x5450a0a8, 0xa5a50000, 0xa0a05050, 0x5555a0a0, 0x5a5a5050, 0xaa550000, 0xaa555500, 0xaaaa5500,
    0x90909090, 0x94949494, 0xa4a4a4a4, 0xa9a59450, 0x2a0a4250, 0xa5945040, 0x0a425054, 0xa5a5a500, 0x55a0a0a0, 0xa8a85454, 0x6a6a4040,
    0xa4a45000, 0x1a1a0500, 0x0050a4a4, 0xaaa59090, 0x14696914, 0x69691400, 0xa08585a0, 0xaa821414, 0x50a4a450, 0x6a5a0200, 0xa9a58000,
    0x5090a0a8, 0xa8a09050, 0x24242424, 0x00aa5500, 0x24924924, 0x24499224, 0x50a50a50, 0x500aa550, 0xaaaa4444, 0x66660000, 0xa5a0a5a0,
    0x50a050a0, 0x69286928, 0x44aaaa44, 0x66666600, 0xaa444444, 0x54a854a8, 0x95809580, 0x96969600, 0xa85454a8, 0x80959580, 0xaa141414,
    0x96960000, 0xaaaa1414, 0xa05050a0, 0xa0a5a5a0, 0x96000000, 0x40804080, 0xa9a8a9a8, 0xaaaaaa44, 0x2a4a5254,
];
/// anchor texel of subset 1 in a 2-subset partition (subset 0's is texel 0)
pub const ANCHOR2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8, 2, 2, 8, 8, 2, 2, 15, 15, 6, 8, 2, 8,
    15, 15, 2, 8, 2, 2, 2, 15, 15, 6, 6, 2, 6, 8, 15, 15, 2, 2, 15, 15, 15, 15, 15, 2, 2, 15,
];
/// anchor texels of subsets 1 and 2 in a 3-subset partition
pub const ANCHOR3: [[u8; 2]; 64] = [
    [3, 15], [3, 8], [15, 8], [15, 3], [8, 15], [3, 15], [15, 3], [15, 8], [8, 15], [8, 15], [6, 15], [6, 15], [6, 15], [5, 15], [3, 15],
    [3, 8], [3, 15], [3, 8], [8, 15], [15, 3], [3, 15], [3, 8], [6, 15], [10, 8], [5, 3], [8, 15], [8, 6], [6, 10], [8, 15], [5, 15],
    [15, 10], [15, 8], [8, 15], [15, 3], [3, 15], [5, 10], [6, 10], [10, 8], [8, 9], [15, 10], [15, 6], [3, 15], [15, 8], [5, 15],
    [15, 3], [15, 6], [15, 6], [15, 8], [3, 15], [15, 3], [5, 15], [5, 15], [5, 15], [8, 15], [5, 15], [10, 15], [5, 15], [10, 15],
    [8, 15], [13, 15], [15, 3], [12, 15], [3, 15], [3, 8],
];

const W2: [u32; 4] = [0, 21, 43, 64];
const W3: [u32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const W4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// Per-mode parameters: `pkind` is the p-bit kind (0 none, 1 per endpoint, 2 shared per subset); `ib` the index width (modes 4, 5:
/// the colour set unless the selector swaps them) and `ib2` the width of the separate second (alpha) index set, 0 if none.
struct Mode {
    subsets: usize,
    pbits: u32,
    rot: bool,
    isel: bool,
    cb: u32,
    ab: u32,
    pkind: u8,
    ib: u32,
    ib2: u32,
}

const MODES: [Mode; 8] = [
    Mode { subsets: 3, pbits: 4, rot: false, isel: false, cb: 4, ab: 0, pkind: 1, ib: 3, ib2: 0 },
    Mode { subsets: 2, pbits: 6, rot: false, isel: false, cb: 6, ab: 0, pkind: 2, ib: 3, ib2: 0 },
    Mode { subsets: 3, pbits: 6, rot: false, isel: false, cb: 5, ab: 0, pkind: 0, ib: 2, ib2: 0 },
    Mode { subsets: 2, pbits: 6, rot: false, isel: false, cb: 7, ab: 0, pkind: 1, ib: 2, ib2: 0 },
    Mode { subsets: 1, pbits: 0, rot: true, isel: true, cb: 5, ab: 6, pkind: 0, ib: 2, ib2: 3 },
    Mode { subsets: 1, pbits: 0, rot: true, isel: false, cb: 7, ab: 8, pkind: 0, ib: 2, ib2: 2 },
    Mode { subsets: 1, pbits: 0, rot: false, isel: false, cb: 7, ab: 7, pkind: 1, ib: 4, ib2: 0 },
    Mode { subsets: 2, pbits: 6, rot: false, isel: false, cb: 5, ab: 5, pkind: 1, ib: 2, ib2: 0 },
];

struct Bits(u128, u32);

impl Bits {
    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = ((self.0 >> self.1) & ((1u128 << n) - 1)) as u32;
        self.1 += n;
        v
    }
}

/// Replicates the top bits of an `bits`-bit value (p-bit included, 4..=8) up to 8 bits.
fn expand(v: u32, bits: u32) -> u32 {
    (v << (8 - bits)) | (v >> (2 * bits - 8))
}

fn lerp(e0: u32, e1: u32, w: u32) -> u32 {
    (e0 * (64 - w) + e1 * w + 32) >> 6
}

fn weight(ib: u32, i: usize) -> u32 {
    match ib {
        2 => W2[i],
        3 => W3[i],
        _ => W4[i],
    }
}

/// Decodes one 16-byte block to 16 RGBA texels (row-major 4x4).
pub fn decode_block(blk: &[u8], out: &mut [[u8; 4]; 16]) {
    let v = u128::from_le_bytes(blk[..16].try_into().unwrap());
    let mode = (v as u8).trailing_zeros() as usize;
    if mode >= 8 {
        *out = [[0; 4]; 16];
        return;
    }
    let m = &MODES[mode];
    let mut b = Bits(v, mode as u32 + 1);
    let part = b.get(m.pbits) as usize;
    let rot = if m.rot { b.get(2) } else { 0 };
    let isel = m.isel && b.get(1) == 1;
    let ne = m.subsets * 2;
    let mut ep = [[0u32; 4]; 6];
    for c in 0..3 {
        for e in ep.iter_mut().take(ne) {
            e[c] = b.get(m.cb);
        }
    }
    if m.ab > 0 {
        for e in ep.iter_mut().take(ne) {
            e[3] = b.get(m.ab);
        }
    }
    let (mut cbits, mut abits) = (m.cb, m.ab);
    match m.pkind {
        1 => {
            for e in ep.iter_mut().take(ne) {
                let p = b.get(1);
                for c in 0..if m.ab > 0 { 4 } else { 3 } {
                    e[c] = (e[c] << 1) | p;
                }
            }
            cbits += 1;
            abits += (m.ab > 0) as u32;
        }
        2 => {
            for s in 0..m.subsets {
                let p = b.get(1);
                for e in &mut ep[2 * s..2 * s + 2] {
                    for c in 0..3 {
                        e[c] = (e[c] << 1) | p;
                    }
                }
            }
            cbits += 1;
        }
        _ => {}
    }
    for e in ep.iter_mut().take(ne) {
        for c in 0..3 {
            e[c] = expand(e[c], cbits);
        }
        e[3] = if m.ab > 0 { expand(e[3], abits) } else { 255 };
    }
    let subset_of = |i: usize| -> usize {
        match m.subsets {
            1 => 0,
            2 => ((PART2[part] >> i) & 1) as usize,
            _ => ((PART3[part] >> (2 * i)) & 3) as usize,
        }
    };
    let is_anchor = |i: usize| -> bool {
        match m.subsets {
            1 => i == 0,
            2 => i == 0 || i == ANCHOR2[part] as usize,
            _ => i == 0 || i == ANCHOR3[part][0] as usize || i == ANCHOR3[part][1] as usize,
        }
    };
    let mut idx = [0usize; 16];
    for (i, x) in idx.iter_mut().enumerate() {
        *x = b.get(m.ib - is_anchor(i) as u32) as usize;
    }
    let mut idx2 = [0usize; 16];
    if m.ib2 > 0 {
        for (i, x) in idx2.iter_mut().enumerate() {
            *x = b.get(m.ib2 - (i == 0) as u32) as usize;
        }
    }
    for i in 0..16 {
        let s = subset_of(i);
        let (e0, e1) = (ep[2 * s], ep[2 * s + 1]);
        let mut px = [0u32; 4];
        if m.ib2 > 0 {
            // separate colour and alpha index sets; the selector says which set drives which
            let (ci, cib, ai, aib) = if isel { (idx2[i], m.ib2, idx[i], m.ib) } else { (idx[i], m.ib, idx2[i], m.ib2) };
            let (cw, aw) = (weight(cib, ci), weight(aib, ai));
            for c in 0..3 {
                px[c] = lerp(e0[c], e1[c], cw);
            }
            px[3] = lerp(e0[3], e1[3], aw);
        } else {
            let w = weight(m.ib, idx[i]);
            for c in 0..4 {
                px[c] = lerp(e0[c], e1[c], w);
            }
        }
        if rot > 0 {
            px.swap(3, rot as usize - 1);
        }
        out[i] = [px[0] as u8, px[1] as u8, px[2] as u8, px[3] as u8];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LSB-first bit writer for hand-built blocks
    struct W(u128, u32);
    impl W {
        fn new() -> W {
            W(0, 0)
        }
        fn put(&mut self, v: u32, n: u32) -> &mut W {
            self.0 |= (v as u128) << self.1;
            self.1 += n;
            self
        }
        fn block(&self) -> [u8; 16] {
            assert_eq!(self.1, 128, "block must be exactly 128 bits");
            self.0.to_le_bytes()
        }
    }

    fn dec(b: [u8; 16]) -> [[u8; 4]; 16] {
        let mut o = [[0u8; 4]; 16];
        decode_block(&b, &mut o);
        o
    }

    #[test]
    fn tables_are_consistent() {
        for p in 0..64 {
            // texel 0 is in subset 0; the anchors lie in their own subsets
            assert_eq!(PART2[p] & 1, 0);
            assert_eq!((PART2[p] >> ANCHOR2[p]) & 1, 1, "partition {}", p);
            assert_eq!(PART3[p] & 3, 0);
            for s in 0..2 {
                assert_eq!((PART3[p] >> (2 * ANCHOR3[p][s])) & 3, s as u32 + 1, "partition {} subset {}", p, s + 1);
            }
        }
        // well-known entries of the published tables
        assert_eq!(PART2[0], 0xcccc); // 0011 0011 0011 0011
        assert_eq!(ANCHOR2[17], 2);
        assert_eq!(ANCHOR3[0], [3, 15]);
    }

    #[test]
    fn mode0_three_subsets_with_pbits() {
        // partition 0; 4-bit endpoints with a p-bit each; every index all ones
        let mut w = W::new();
        w.put(1, 1).put(0, 4); // mode 0, partition 0
        for _ in 0..3 {
            for e in [0, 15, 15, 15, 0, 0] {
                w.put(e, 4);
            }
        }
        for p in [0, 1, 1, 1, 0, 0] {
            w.put(p, 1);
        }
        for i in 0..16 {
            // anchors of partition 0 are texels 0, 3, 15 and store 2 bits
            let n = if i == 0 || i == 3 || i == 15 { 2 } else { 3 };
            w.put((1 << n) - 1, n);
        }
        let px = dec(w.block());
        // texel 1 (subset 0): e1 = 15<<1|1 = 31 -> 255 at weight 64; texel 0 is an anchor: index 3 -> weight 27 from e0 = 0
        assert_eq!(px[1], [255, 255, 255, 255]);
        assert_eq!(px[0][0] as u32, (255 * 27 + 32) >> 6);
    }

    #[test]
    fn mode1_shared_pbit_and_interpolation() {
        // partition 0 (subset 0 = columns 0,1), 6-bit endpoints, p-bit shared per subset
        let mut w = W::new();
        w.put(2, 2).put(0, 6);
        for _ in 0..3 {
            for e in [0, 63, 32, 32] {
                w.put(e, 6);
            }
        }
        w.put(1, 1).put(0, 1); // subset 0 p-bit 1, subset 1 p-bit 0
        for i in 0..16 {
            let n = if i == 0 || i == 15 { 2 } else { 3 };
            w.put((i % (1 << n)) as u32, n);
        }
        let px = dec(w.block());
        // subset 0: e0 = (0<<1|1) = 1 -> 2 ; e1 = 127 -> 254|1 = 255.  Texel 1 index 1 -> weight 9
        assert_eq!(px[0], [2, 2, 2, 255]);
        assert_eq!(px[1][0] as u32, (2 * 55 + 255 * 9 + 32) >> 6);
        // subset 1 (texel 2): e = 32<<1|0 = 64 -> 64<<1 | 64>>6 = 129
        assert_eq!(px[2], [129, 129, 129, 255]);
    }

    #[test]
    fn mode2_five_bit_no_pbits() {
        let mut w = W::new();
        w.put(4, 3).put(0, 6);
        for _ in 0..3 {
            for e in [31, 31, 0, 0, 16, 16] {
                w.put(e, 5);
            }
        }
        for i in 0..16 {
            let n = if i == 0 || i == 3 || i == 15 { 1 } else { 2 };
            w.put(0, n);
        }
        let px = dec(w.block());
        assert_eq!(px[0], [255, 255, 255, 255]); // subset 0
        assert_eq!((PART3[0] >> 6) & 3, 1);
        assert_eq!(px[3], [0, 0, 0, 255]); // texel 3: subset 1
        // 16 -> (16<<3)|(16>>2) = 132
        let sub2 = (0..16).find(|&i| (PART3[0] >> (2 * i)) & 3 == 2).unwrap();
        assert_eq!(px[sub2], [132, 132, 132, 255]);
    }

    #[test]
    fn mode3_seven_bit_with_endpoint_pbits() {
        let mut w = W::new();
        w.put(8, 4).put(1, 6); // mode 3, partition 1 (0x8888: column 3 is subset 1)
        for _ in 0..3 {
            for e in [127, 0, 64, 64] {
                w.put(e, 7);
            }
        }
        for p in [1, 0, 0, 1] {
            w.put(p, 1);
        }
        for i in 0..16 {
            let n = if i == 0 || i == 15 { 1 } else { 2 }; // ANCHOR2[1] = 15
            w.put(if i == 5 { 1 } else { 0 }, n);
        }
        let px = dec(w.block());
        assert_eq!(px[0], [255, 255, 255, 255]); // e0 = 127<<1|1 = 255
        assert_eq!(px[5][0] as u32, (255 * (64 - 21) + 32) >> 6); // 2-bit index 1: weight 21 towards e1 = 0
        assert_eq!(px[3], [128, 128, 128, 255]); // subset 1: 64<<1|0 = 128
    }

    #[test]
    fn mode4_rotation_and_index_selector() {
        // 5-bit colour (0 .. 31), 6-bit alpha (0 .. 63); colour index 2 bit, alpha index 3 bit; texel 1: colour idx 1, alpha idx 7
        let build = |rot: u32, isel: u32| {
            let mut w = W::new();
            w.put(16, 5).put(rot, 2).put(isel, 1);
            for _ in 0..3 {
                w.put(0, 5).put(31, 5);
            }
            w.put(0, 6).put(63, 6);
            for i in 0..16 {
                w.put(if i == 1 { 1 } else { 0 }, if i == 0 { 1 } else { 2 });
            }
            for i in 0..16 {
                w.put(if i == 1 { 7 } else { 0 }, if i == 0 { 2 } else { 3 });
            }
            w.block()
        };
        let g = ((255 * 21 + 32) >> 6) as u8; // 84
        assert_eq!(dec(build(0, 0))[1], [g, g, g, 255]);
        assert_eq!(dec(build(0, 0))[0], [0, 0, 0, 0]);
        // rotation 1 swaps R and A
        assert_eq!(dec(build(1, 0))[1], [255, g, g, g]);
        // selector 1: the 3-bit set drives colour (index 7 -> 255), the 2-bit set drives alpha (index 1 -> weight 21)
        assert_eq!(dec(build(0, 1))[1], [255, 255, 255, g]);
    }

    #[test]
    fn mode5_separate_alpha() {
        let mut w = W::new();
        w.put(32, 6).put(3, 2); // mode 5, rotation 3 swaps B and A
        for c in 0..3 {
            w.put(0, 7).put(if c == 2 { 127 } else { 0 }, 7); // only blue goes 0 -> 255
        }
        w.put(255, 8).put(0, 8); // alpha 255 -> 0
        for i in 0..16 {
            w.put(if i == 1 { 3 } else { 0 }, if i == 0 { 1 } else { 2 });
        }
        for i in 0..16 {
            w.put(if i == 1 { 2 } else { 0 }, if i == 0 { 1 } else { 2 });
        }
        let px = dec(w.block());
        // texel 1: colour weight 64 -> blue 255; alpha weight 43 -> 255 * 21 / 64 = 84; rotated: B = alpha, A = blue
        assert_eq!(px[1], [0, 0, 84, 255]);
        assert_eq!(px[0], [0, 0, 255, 0]);
    }

    #[test]
    fn mode6_with_pbits() {
        let mut w = W::new();
        w.put(64, 7);
        for c in 0..4 {
            w.put(if c == 3 { 100 } else { 10 * (c + 1) }, 7).put(127, 7);
        }
        w.put(1, 1).put(0, 1);
        for i in 0..16 {
            w.put(if i == 5 { 15 } else { 0 }, if i == 0 { 3 } else { 4 });
        }
        let px = dec(w.block());
        // e0 = v<<1|1, e1 = 127<<1|0 = 254
        assert_eq!(px[0], [21, 41, 61, 201]);
        assert_eq!(px[5], [254, 254, 254, 254]);
        assert_eq!(px[7], [21, 41, 61, 201]);
    }

    #[test]
    fn mode7_two_subsets_with_alpha() {
        let mut w = W::new();
        w.put(128, 8).put(1, 6); // partition 1
        for _ in 0..3 {
            for e in [31, 0, 0, 31] {
                w.put(e, 5);
            }
        }
        for e in [31, 31, 0, 0] {
            w.put(e, 5); // alpha
        }
        for p in [0, 0, 1, 1] {
            w.put(p, 1);
        }
        for i in 0..16 {
            w.put(if i == 1 { 2 } else { 0 }, if i == 0 || i == 15 { 1 } else { 2 });
        }
        let px = dec(w.block());
        // 5 bit + p-bit = 6 bits.  subset 0: e0 = 31<<1|0 = 62 -> 251, e1 = 0, alpha 251 both
        assert_eq!(px[0], [251, 251, 251, 251]);
        assert_eq!(px[1][0] as u32, (251 * 21 + 32) >> 6); // index 2 -> weight 43
        // subset 1 (texel 3): e0 = 0<<1|1 = 1 -> 4; alpha 0<<1|1 -> 4
        assert_eq!(px[3], [4, 4, 4, 4]);
    }

    #[test]
    fn reserved_block_is_transparent_black() {
        assert_eq!(dec([0; 16]), [[0; 4]; 16]);
    }

    #[test]
    fn image_decode_and_png_roundtrip() {
        // 6 x 5 image = 2 x 2 blocks, two mode-6 blocks of flat colour and two reserved ones; edges are cropped
        let flat = |r: u32, g: u32, b: u32| {
            let mut w = W::new();
            w.put(64, 7);
            for c in [r, g, b, 127] {
                w.put(c, 7).put(c, 7);
            }
            w.put(0, 2).put(0, 63);
            w.block()
        };
        let data: Vec<u8> = [flat(127, 0, 0), flat(0, 127, 0), [0; 16], flat(0, 0, 127)].concat();
        let px = crate::dds::decode("bc7", &data, 6, 5).unwrap();
        assert_eq!(px.len(), 6 * 5 * 4);
        assert_eq!(&px[0..4], &[254, 0, 0, 254]); // 7-bit 127 with p-bit 0 = 254
        assert_eq!(&px[(0 * 6 + 5) * 4..(0 * 6 + 5) * 4 + 4], &[0, 254, 0, 254]); // second block, last column
        assert_eq!(&px[(4 * 6) * 4..(4 * 6) * 4 + 4], &[0, 0, 0, 0]); // reserved block
        assert_eq!(&px[(4 * 6 + 5) * 4..(4 * 6 + 5) * 4 + 4], &[0, 0, 254, 254]);
        let png = crate::png::png_rgba(6, 5, &px);
        assert_eq!(crate::png::decode_png_rgba(&png).unwrap(), (6, 5, px));
    }

    /// Compares every texel with Pillow's BC7 decoder on random blocks of all modes (except the reserved one: Pillow returns opaque black, the spec transparent black).  The reference files come from
    /// `$P4K_BC7_REF/blocks.bin` (16 bytes per block) and `rgba.bin` (Pillow's RGBA of the 4-block-wide image); skipped without.
    #[test]
    fn matches_reference_decoder_on_random_blocks() {
        let Ok(dir) = std::env::var("P4K_BC7_REF") else { return };
        let blocks = std::fs::read(format!("{}/blocks.bin", dir)).unwrap();
        let rgba = std::fs::read(format!("{}/rgba.bin", dir)).unwrap();
        let n = blocks.len() / 16;
        let (bw, bh) = (100, n / 100);
        let ours = crate::dds::decode("bc7", &blocks, bw * 4, bh * 4).unwrap();
        assert_eq!(ours.len(), rgba.len());
        let bad = (0..n).filter(|&b| blocks[b * 16] != 0 && (0..16).any(|t| {
            let (x, y) = ((b % bw) * 4 + (t & 3), (b / bw) * 4 + (t >> 2));
            ours[(y * bw * 4 + x) * 4..][..4] != rgba[(y * bw * 4 + x) * 4..][..4]
        })).count();
        assert_eq!(bad, 0, "{} of {} blocks differ", bad, n);
    }
}
