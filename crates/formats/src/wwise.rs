//! Wwise Vorbis (.wem, RIFF format tag 0xFFFF) to standard Ogg Vorbis.
//!
//! All 117,067 files under Data/Sounds/wwise/Media use this codec.  Layout (CONFIRMED against the engine's Wwise Vorbis source and
//! by a byte comparison on 400 files unless marked):
//!
//! * `fmt ` payload (size 0x42): +0x02 u16 channels, +0x04 u32 sample rate, +0x08 u32 average bytes/s, +0x18 u32 total PCM frames,
//!   +0x28 u32 seek table size, +0x2C u32 offset of the first audio packet in the data chunk, +0x30 u16 max packet size,
//!   +0x40 u8 blocksize0 exponent, +0x41 u8 blocksize1 exponent.
//! * `data` payload: seek table, u16 setup size S, S setup bytes, then audio packets, each u16 size + payload.
//! * The setup is the standard Vorbis setup with the codebooks replaced by 10-bit ids into the engine's packed codebook library
//!   (aoTuV 603, 598 books), no time-domain section, no framing bit, modes without window/transform type; each mapping submap still
//!   carries its time byte (EMPIRICAL).
//! * Audio packets omit the packet-type bit and, for long blocks, the previous/next window flags (ww2ogg's "modified packets" form);
//!   the flags are rebuilt from the neighbouring packets (INFERRED from ww2ogg; verified by decoding with ffmpeg).
//!
//! The page layout is deterministic, so the same input always gives the same bytes.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::error::{u16_at, u32_at, Error, Result};

fn werr(s: impl Into<String>) -> Error {
    Error::msg(s)
}

/// Number of bits needed for the absolute value (`int.bit_length()`).
pub fn ilog(v: i64) -> u32 {
    64 - v.unsigned_abs().leading_zeros()
}

/// LSB-first bit reader (the Vorbis / Wwise bit order).
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn read(&mut self, n: u32) -> Result<u64> {
        if n == 0 {
            return Ok(0);
        }
        let end = self.pos + n as usize;
        if end > self.d.len() * 8 {
            return Err(werr("setup data ends early"));
        }
        let mut v = 0u64;
        for i in 0..n as usize {
            let p = self.pos + i;
            v |= (((self.d[p >> 3] >> (p & 7)) & 1) as u64) << i;
        }
        self.pos = end;
        Ok(v)
    }
}

/// LSB-first bit writer.
#[derive(Default)]
struct Out {
    buf: Vec<u8>,
    n: usize,
}

impl Out {
    fn write(&mut self, value: u64, bits: u32) {
        for i in 0..bits as usize {
            if self.n & 7 == 0 {
                self.buf.push(0);
            }
            if i < 64 && (value >> i) & 1 == 1 {
                let last = self.buf.len() - 1;
                self.buf[last] |= 1 << (self.n & 7);
            }
            self.n += 1;
        }
    }
    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Vorbis' _book_maptype1_quantvals: the largest v with v**dims <= entries.
fn quantvals(entries: u64, dims: u32) -> Result<u64> {
    if entries == 0 {
        return Err(werr("negative shift count"));
    }
    let shift = ((ilog(entries as i64) as u64 - 1) * (dims as u64).saturating_sub(1)) / (dims as u64).max(1);
    let mut vals = entries >> shift;
    loop {
        let (mut acc, mut acc1) = (1u128, 1u128);
        for _ in 0..dims {
            acc = acc.saturating_mul(vals as u128);
            acc1 = acc1.saturating_mul(vals as u128 + 1);
        }
        if acc <= entries as u128 && (entries as u128) < acc1 {
            return Ok(vals);
        }
        if acc > entries as u128 {
            vals -= 1;
        } else {
            vals += 1;
        }
    }
}

/// Write one packed book (the engine's compact form) as a standard Vorbis codebook.
fn unpack_codebook(packed: &[u8], out: &mut Out) -> Result<()> {
    let mut r = Bits { d: packed, pos: 0 };
    let dims = r.read(4)? as u32;
    let entries = r.read(14)?;
    out.write(0x564342, 24);
    out.write(dims as u64, 16);
    out.write(entries, 24);
    let ordered = r.read(1)?;
    out.write(ordered, 1);
    if ordered != 0 {
        out.write(r.read(5)?, 5);
        let mut cur = 0u64;
        while cur < entries {
            let bits = ilog((entries - cur) as i64);
            let n = r.read(bits)?;
            out.write(n, bits);
            cur += n;
        }
    } else {
        let cll = r.read(3)?;
        if cll == 0 || cll > 5 {
            return Err(werr(format!("bad codeword length length {}", cll)));
        }
        let sparse = r.read(1)?;
        out.write(sparse, 1);
        for _ in 0..entries {
            let mut present = 1;
            if sparse != 0 {
                present = r.read(1)?;
                out.write(present, 1);
            }
            if present != 0 {
                out.write(r.read(cll as u32)?, 5);
            }
        }
    }
    let lookup = r.read(1)?;
    out.write(lookup, 4);
    if lookup == 1 {
        out.write(r.read(32)?, 32);
        out.write(r.read(32)?, 32);
        let vb = r.read(4)?;
        out.write(vb, 4);
        out.write(r.read(1)?, 1);
        for _ in 0..quantvals(entries, dims)? {
            out.write(r.read(vb as u32 + 1)?, vb as u32 + 1);
        }
    } else if lookup != 0 {
        return Err(werr(format!("bad lookup type {}", lookup)));
    }
    Ok(())
}

/// Standard Vorbis setup header body (after the 'vorbis' tag) from the stripped Wwise setup: (bytes, mode flags).
pub fn rebuild_setup(setup: &[u8], channels: u32, codebooks: &[&[u8]]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut r = Bits { d: setup, pos: 0 };
    let mut out = Out::default();
    let count = r.read(8)? + 1;
    out.write(count - 1, 8);
    for _ in 0..count {
        let i = r.read(10)? as usize;
        if i >= codebooks.len() {
            return Err(werr(format!("codebook id {} not in the library", i)));
        }
        unpack_codebook(codebooks[i], &mut out)?;
    }
    out.write(0, 6); // time-domain transforms: one placeholder
    out.write(0, 16);
    let nfloor = r.read(6)? + 1;
    out.write(nfloor - 1, 6);
    for _ in 0..nfloor {
        out.write(1, 16);
        let parts = r.read(5)?;
        out.write(parts, 5);
        let mut classes = Vec::new();
        let mut top = 0u64;
        for _ in 0..parts {
            let c = r.read(4)?;
            out.write(c, 4);
            classes.push(c);
            top = top.max(c);
        }
        let mut dim: HashMap<u64, u64> = HashMap::new();
        for j in 0..=top {
            let d = r.read(3)? + 1;
            dim.insert(j, d);
            out.write(d - 1, 3);
            let sub = r.read(2)?;
            out.write(sub, 2);
            if sub != 0 {
                out.write(r.read(8)?, 8);
            }
            for _ in 0..(1u32 << sub) {
                out.write(r.read(8)?, 8);
            }
        }
        out.write(r.read(2)?, 2);
        let rb = r.read(4)? as u32;
        out.write(rb as u64, 4);
        for c in &classes {
            for _ in 0..dim[c] {
                out.write(r.read(rb)?, rb);
            }
        }
    }
    let nres = r.read(6)? + 1;
    out.write(nres - 1, 6);
    for _ in 0..nres {
        out.write(r.read(2)?, 16);
        let mut ncls = 0;
        for w in [24u32, 24, 24, 6, 8] {
            let v = r.read(w)?;
            out.write(v, w);
            if w == 6 {
                ncls = v + 1;
            }
        }
        let mut cascade = Vec::new();
        for _ in 0..ncls {
            let low = r.read(3)?;
            let flag = r.read(1)?;
            out.write(low, 3);
            out.write(flag, 1);
            let mut high = 0;
            if flag != 0 {
                high = r.read(5)?;
                out.write(high, 5);
            }
            cascade.push(high << 3 | low);
        }
        for c in cascade {
            for k in 0..8 {
                if c & (1 << k) != 0 {
                    out.write(r.read(8)?, 8);
                }
            }
        }
    }
    let nmap = r.read(6)? + 1;
    out.write(nmap - 1, 6);
    for _ in 0..nmap {
        out.write(0, 16);
        let flag = r.read(1)?;
        out.write(flag, 1);
        let mut subs = 1;
        if flag != 0 {
            subs = r.read(4)? + 1;
            out.write(subs - 1, 4);
        }
        let sq = r.read(1)?;
        out.write(sq, 1);
        if sq != 0 {
            let steps = r.read(8)? + 1;
            out.write(steps - 1, 8);
            let w = ilog(channels as i64 - 1);
            for _ in 0..steps {
                out.write(r.read(w)?, w);
                out.write(r.read(w)?, w);
            }
        }
        if r.read(2)? != 0 {
            return Err(werr("mapping reserved bits set"));
        }
        out.write(0, 2);
        if subs > 1 {
            for _ in 0..channels {
                out.write(r.read(4)?, 4);
            }
        }
        for _ in 0..subs {
            r.read(8)?; // the stripped setup keeps the unused time-domain byte of every submap (EMPIRICAL)
            out.write(0, 8);
            out.write(r.read(8)?, 8);
            out.write(r.read(8)?, 8);
        }
    }
    let nmode = r.read(6)? + 1;
    out.write(nmode - 1, 6);
    let mut flags = Vec::new();
    for _ in 0..nmode {
        let f = r.read(1)?;
        flags.push(f as u8);
        out.write(f, 1);
        out.write(0, 32); // window type and transform type, 16 bits each
        out.write(r.read(8)?, 8);
    }
    out.write(1, 1); // framing
    Ok((out.into_bytes(), flags))
}

/// A parsed Wwise Vorbis .wem.
#[derive(Debug, Clone)]
pub struct Wem<'a> {
    pub channels: u32,
    pub rate: u32,
    pub avg: u32,
    pub samples: u32,
    pub b0: u8,
    pub b1: u8,
    pub setup: &'a [u8],
    pub packets: Vec<&'a [u8]>,
}

/// Slice that clamps at the end.
fn slice(d: &[u8], a: usize, n: usize) -> &[u8] {
    let a = a.min(d.len());
    &d[a..a.saturating_add(n).min(d.len())]
}

pub fn parse_wem(data: &[u8]) -> Result<Wem<'_>> {
    if data.get(..4) != Some(b"RIFF") || data.get(8..12) != Some(b"WAVE") {
        return Err(werr("not a RIFF/WAVE file"));
    }
    let (mut pos, mut fmt, mut body) = (12usize, None, None);
    while pos + 8 <= data.len() {
        let tag = &data[pos..pos + 4];
        let size = u32_at(data, pos + 4)? as usize;
        if tag == b"fmt " {
            fmt = Some(slice(data, pos + 8, size));
        } else if tag == b"data" {
            body = Some(slice(data, pos + 8, size));
        }
        pos += 8 + size + (size & 1);
    }
    let (Some(fmt), Some(body)) = (fmt, body) else { return Err(werr("no Wwise Vorbis fmt/data chunks")) };
    if fmt.len() < 0x42 {
        return Err(werr("no Wwise Vorbis fmt/data chunks"));
    }
    let tag = u16_at(fmt, 0)?;
    if tag != 0xFFFF {
        return Err(werr(format!("format tag {:#x} is not Wwise Vorbis", tag)));
    }
    let channels = u16_at(fmt, 2)? as u32;
    let rate = u32_at(fmt, 4)?;
    let avg = u32_at(fmt, 8)?;
    let samples = u32_at(fmt, 0x18)?;
    let seek = u32_at(fmt, 0x28)? as usize;
    let first = u32_at(fmt, 0x2C)? as usize;
    let maxp = u16_at(fmt, 0x30)? as usize;
    let (b0, b1) = (fmt[0x40], fmt[0x41]);
    let ssize = u16_at(body, seek)? as usize;
    let setup = slice(body, seek + 2, ssize);
    if seek + 2 + ssize != first {
        return Err(werr("setup does not end at the first audio packet"));
    }
    let mut packets = Vec::new();
    let mut p = first;
    while p + 2 <= body.len() {
        let n = u16_at(body, p)? as usize;
        if n == 0 || n > maxp || p + 2 + n > body.len() {
            return Err(werr(format!("bad audio packet size {} at {}", n, p)));
        }
        packets.push(&body[p + 2..p + 2 + n]);
        p += 2 + n;
    }
    Ok(Wem { channels, rate, avg, samples, b0, b1, setup, packets })
}

fn crc_table() -> &'static [u32; 256] {
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04C1_1DB7 } else { r << 1 };
            }
            *slot = r;
        }
        t
    })
}

fn crc(b: &[u8]) -> u32 {
    let t = crc_table();
    let mut c = 0u32;
    for &x in b {
        c = (c << 8) ^ t[((c >> 24) as u8 ^ x) as usize];
    }
    c
}

/// One Ogg page holding whole packets.
fn page(serial: u32, seq: u32, granule: i64, flags: u8, packets: &[Vec<u8>]) -> Vec<u8> {
    let mut segs = Vec::new();
    for p in packets {
        segs.extend(std::iter::repeat(0xFFu8).take(p.len() / 255));
        segs.push((p.len() % 255) as u8);
    }
    let mut b = Vec::with_capacity(27 + segs.len() + packets.iter().map(Vec::len).sum::<usize>());
    b.extend_from_slice(b"OggS");
    b.push(0);
    b.push(flags);
    b.extend_from_slice(&granule.to_le_bytes());
    b.extend_from_slice(&serial.to_le_bytes());
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.push(segs.len() as u8);
    b.extend_from_slice(&segs);
    for p in packets {
        b.extend_from_slice(p);
    }
    let c = crc(&b);
    b[22..26].copy_from_slice(&c.to_le_bytes());
    b
}

/// Ogg Vorbis bytes for a Wwise Vorbis .wem (any serial works; 0x57454D, "WEM", is the conventional one).
pub fn wem_to_ogg(data: &[u8], codebooks: &[&[u8]], serial: u32) -> Result<Vec<u8>> {
    let w = parse_wem(data)?;
    let ch = w.channels;
    let (setup, flags) = rebuild_setup(w.setup, ch, codebooks)?;
    let mode_bits = ilog(flags.len() as i64 - 1);
    let mut ident = b"\x01vorbis".to_vec();
    ident.extend_from_slice(&0u32.to_le_bytes());
    ident.push(ch as u8);
    ident.extend_from_slice(&w.rate.to_le_bytes());
    ident.extend_from_slice(&0i32.to_le_bytes());
    ident.extend_from_slice(&((w.avg as i64 * 8) as i32).to_le_bytes());
    ident.extend_from_slice(&0i32.to_le_bytes());
    ident.push(w.b0 | w.b1 << 4);
    ident.push(1);
    let mut comment = b"\x03vorbis".to_vec();
    comment.extend_from_slice(&8u32.to_le_bytes());
    comment.extend_from_slice(b"P4kTools");
    comment.extend_from_slice(&0u32.to_le_bytes());
    comment.push(1);
    let mut setup_pkt = b"\x05vorbis".to_vec();
    setup_pkt.extend_from_slice(&setup);
    let mut pages = page(serial, 0, 0, 2, &[ident]);
    let mut seq = 1;
    pages.extend(page(serial, seq, 0, 0, &[comment, setup_pkt]));
    seq += 1;
    let pk = &w.packets;
    let mask = (1u32 << mode_bits) - 1;
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(pk.len());
    let mut sizes = Vec::with_capacity(pk.len());
    let mut prev_long = 0u64;
    for (i, p) in pk.iter().enumerate() {
        let mode = (p[0] as u32 & mask) as usize;
        if mode >= flags.len() {
            return Err(werr(format!("bad mode {}", mode)));
        }
        let long = flags[mode] != 0;
        let mut o = Out::default();
        o.write(0, 1); // bit 0 = packet type 0
        o.write(mode as u64, mode_bits);
        if long {
            let nxt = if i + 1 < pk.len() && ((pk[i + 1][0] as u32 & mask) as usize) < flags.len() {
                flags[(pk[i + 1][0] as u32 & mask) as usize] as u64
            } else {
                0
            };
            o.write(prev_long, 1);
            o.write(nxt, 1);
        }
        o.write((p[0] >> mode_bits) as u64, 8 - mode_bits);
        for &b in &p[1..] {
            o.write(b as u64, 8);
        }
        out.push(o.into_bytes());
        sizes.push(if long { w.b1 } else { w.b0 });
        prev_long = long as u64;
    }
    // granule position of each packet's end: a packet completes (previous window / 4 + this window / 4) samples
    let mut gran = Vec::with_capacity(sizes.len());
    let (mut total, mut prev) = (0i64, None::<u8>);
    for &s in &sizes {
        if let Some(pv) = prev {
            total += (1i64 << pv) / 4 + (1i64 << s) / 4;
        }
        gran.push(total);
        prev = Some(s);
    }
    let mut i = 0;
    while i < out.len() {
        let (mut j, mut segs) = (i, 0usize);
        while j < out.len() && segs + out[j].len() / 255 + 1 <= 250 && j - i < 32 {
            segs += out[j].len() / 255 + 1;
            j += 1;
        }
        if j == i {
            return Err(werr("packet too large"));
        }
        let last = j == out.len();
        let g = if last { gran[j - 1].min(w.samples as i64) } else { gran[j - 1] };
        pages.extend(page(serial, seq, g, if last { 4 } else { 0 }, &out[i..j]));
        seq += 1;
        i = j;
    }
    Ok(pages)
}
