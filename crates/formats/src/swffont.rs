//! Glyph outlines of the fonts embedded in the game's Scaleform UI files (SWF / GFX).
//!
//! EVIDENCE.  CONFIRMED on the live archive: `Data/UI/fonts/Shared/fonts_en.swf` (zlib-compressed 'CWS', version 15) holds 29 DefineFont3 tags
//! (tag 75) named by DefineFontName (tag 88); `Data/Localization/english/HUD_Font_LocFont.swf` has Orbitron Light, Glass TTY VT220 and more.  The
//! tag layout follows the public SWF file format specification (DefineFont3: FontId, flags HasLayout / WideOffsets / WideCodes / Italic / Bold,
//! language, name, glyph count, offset table, CodeTableOffset, glyph SHAPEs, code table, then the layout block with ascent, descent, leading and the
//! advance table).  Coordinates are in 1/20480 em units, y grows downwards.  Not decoded: kerning, alignment zones, bitmap fonts (tag 10 / 48).
//!
//! `parse_fonts(data)` takes the whole file and returns the fonts with SVG path data (M L Q Z) per character.

use std::collections::HashMap;
use std::io::Read;

use serde_json::{json, Map, Value};

use crate::error::*;
use crate::xmlq::{latin1, split_nul};

pub const EM: f64 = 20480.0;

struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
    bit: u32,
}

impl<'a> Bits<'a> {
    fn u(&mut self, n: u32) -> Result<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self.b.get(self.pos).ok_or_else(|| Error::msg("IndexError: SHAPE data"))?;
            v = (v << 1) | ((byte >> (7 - self.bit)) & 1) as u64;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(v)
    }

    fn s(&mut self, n: u32) -> Result<i64> {
        let v = if n > 0 { self.u(n)? as i64 } else { 0 };
        Ok(if n > 0 && v & (1 << (n - 1)) != 0 { v - (1 << n) } else { v })
    }
}

/// SVG path data of the SHAPE starting at byte `pos` (no style tables: font glyphs use fill 1 only).
fn shape(b: &[u8], pos: usize) -> Result<String> {
    let mut r = Bits { b, pos, bit: 0 };
    let nf = r.u(4)? as u32;
    let nl = r.u(4)? as u32;
    let (mut x, mut y) = (0i64, 0i64);
    let mut out = String::new();
    let mut open = false;
    loop {
        if r.u(1)? == 0 {
            let flags = r.u(5)?;
            if flags == 0 {
                break;
            }
            if flags & 1 != 0 {
                let n = r.u(5)? as u32;
                x = r.s(n)?;
                y = r.s(n)?;
                if open {
                    out.push('Z');
                }
                out.push_str(&format!("M{} {}", x, y));
                open = true;
            }
            if flags & 2 != 0 {
                r.u(nf)?;
            }
            if flags & 4 != 0 {
                r.u(nf)?;
            }
            if flags & 8 != 0 {
                r.u(nl)?;
            }
            if flags & 16 != 0 {
                break;
            }
            continue;
        }
        if r.u(1)? != 0 {
            let n = r.u(4)? as u32 + 2;
            let (dx, dy);
            if r.u(1)? != 0 {
                dx = r.s(n)?;
                dy = r.s(n)?;
            } else if r.u(1)? != 0 {
                dx = 0;
                dy = r.s(n)?;
            } else {
                dx = r.s(n)?;
                dy = 0;
            }
            x += dx;
            y += dy;
            out.push_str(&format!("L{} {}", x, y));
        } else {
            let n = r.u(4)? as u32 + 2;
            let cx = x + r.s(n)?;
            let cy = y + r.s(n)?;
            let ax = cx + r.s(n)?;
            let ay = cy + r.s(n)?;
            out.push_str(&format!("Q{} {} {} {}", cx, cy, ax, ay));
            x = ax;
            y = ay;
        }
    }
    if open {
        out.push('Z');
    }
    Ok(out)
}

/// (tag type, body offset, length) of every tag of a SWF body (after the 8-byte file header).
fn tags(body: &[u8]) -> Result<Vec<(u32, usize, usize)>> {
    let nb = (*body.first().ok_or_else(|| Error::msg("IndexError: empty SWF body"))? >> 3) as usize;
    let mut pos = (5 + nb * 4 + 7) / 8 + 4;
    let mut out = vec![];
    while pos < body.len() {
        let h = u16_at(body, pos)? as u32;
        pos += 2;
        let (t, mut ln) = (h >> 6, (h & 63) as usize);
        if ln == 63 {
            ln = u32_at(body, pos)? as usize;
            pos += 4;
        }
        out.push((t, pos, ln));
        pos += ln;
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    pub name: String,
    pub full_name: String,
    pub bold: bool,
    pub italic: bool,
    pub ascent: i16,
    pub descent: i16,
    pub leading: i16,
    /// character -> (advance, path); a repeated character keeps its first position and its last value (dict semantics)
    pub glyphs: Vec<(char, i16, String)>,
}

impl Font {
    pub fn to_json(&self) -> Value {
        let mut g = Map::new();
        for (c, a, p) in &self.glyphs {
            g.insert(c.to_string(), json!([a, p]));
        }
        json!({"name": self.name, "full_name": self.full_name, "bold": self.bold, "italic": self.italic, "ascent": self.ascent,
            "descent": self.descent, "leading": self.leading, "glyphs": g})
    }
}

fn zlib_decompress(b: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![];
    flate2::read::ZlibDecoder::new(b).read_to_end(&mut out).map_err(|e| Error::msg(format!("zlib.error: {}", e)))?;
    Ok(out)
}

/// Fonts of a SWF / GFX file; empty when the file has none or is not a SWF.
pub fn parse_fonts(data: &[u8]) -> Result<Vec<Font>> {
    let sig = &data[..data.len().min(3)];
    let body: Vec<u8> = if sig == b"CWS" || sig == b"CFX" {
        zlib_decompress(&data[data.len().min(8)..])?
    } else if sig == b"FWS" || sig == b"GFX" {
        data[data.len().min(8)..].to_vec()
    } else {
        return Ok(vec![]);
    };
    let body = &body[..];
    let mut fonts: Vec<(u16, Font)> = vec![];
    let mut names: HashMap<u16, String> = HashMap::new();
    for (t, pos, ln) in tags(body)? {
        if t == 75 {
            let fid = u16_at(body, pos)?;
            let flags = u8_at(body, pos + 2)?;
            let nl = u8_at(body, pos + 4)? as usize;
            let name = latin1(split_nul(crate::ivo::slice(body, pos + 5, pos + 5 + nl), usize::MAX - 1)[0]);
            let mut p = pos + 5 + nl;
            let ng = u16_at(body, p)? as usize;
            p += 2;
            let wide_off = flags & 8 != 0;
            let osz = if wide_off { 4 } else { 2 };
            let rd = |o: usize| -> Result<usize> { Ok(if wide_off { u32_at(body, o)? as usize } else { u16_at(body, o)? as usize }) };
            let offs: Vec<usize> = (0..ng).map(|i| rd(p + i * osz)).collect::<Result<_>>()?;
            let cto = rd(p + ng * osz)?;
            let table = p;
            let cpos = table + cto;
            let wide_codes = flags & 4 != 0;
            let csz = if wide_codes { 2 } else { 1 };
            let codes: Vec<u32> =
                (0..ng).map(|i| Ok(if wide_codes { u16_at(body, cpos + i * 2)? as u32 } else { u8_at(body, cpos + i)? as u32 })).collect::<Result<_>>()?;
            let glyph_paths: Vec<String> = offs.iter().map(|o| shape(body, table + o)).collect::<Result<_>>()?;
            let mut q = cpos + ng * csz;
            let (mut asc, mut desc, mut lead) = (0i16, 0i16, 0i16);
            let mut adv = vec![0i16; ng];
            if flags & 128 != 0 {
                asc = i16_at(body, q)?;
                desc = i16_at(body, q + 2)?;
                lead = i16_at(body, q + 4)?;
                q += 6;
                adv = (0..ng).map(|i| i16_at(body, q + 2 * i)).collect::<Result<_>>()?;
            }
            let mut glyphs: Vec<(char, i16, String)> = vec![];
            for ((c, a), g) in codes.iter().zip(&adv).zip(glyph_paths) {
                if *c == 0 {
                    continue;
                }
                let ch = char::from_u32(*c).unwrap_or('\u{FFFD}');
                match glyphs.iter_mut().find(|e| e.0 == ch) {
                    Some(e) => {
                        e.1 = *a;
                        e.2 = g;
                    }
                    None => glyphs.push((ch, *a, g)),
                }
            }
            let font = Font { full_name: String::new(), name, bold: flags & 1 != 0, italic: flags & 2 != 0, ascent: asc, descent: desc, leading: lead, glyphs };
            match fonts.iter_mut().find(|f| f.0 == fid) {
                Some(f) => f.1 = font,
                None => fonts.push((fid, font)),
            }
        } else if t == 88 {
            let fid = u16_at(body, pos)?;
            names.insert(fid, latin1(split_nul(crate::ivo::slice(body, pos + 2, pos + ln), usize::MAX - 1)[0]));
        }
    }
    Ok(fonts
        .into_iter()
        .map(|(fid, mut f)| {
            f.full_name = names.get(&fid).cloned().unwrap_or_else(|| f.name.clone());
            f
        })
        .collect())
}
