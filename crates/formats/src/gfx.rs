//! Scaleform UI movies (`.gfx` / `.swf`) as a drawable display list: shapes, sprites (timelines), buttons, edit text, exported symbols.
//!
//! EVIDENCE.  EMPIRICAL on the live archive (`Data/UI`, mobiGlas / Kiosk / Frontend / HUD_*): the files are `CFX` (zlib GFX, version 10) or `CWS`
//! (zlib SWF, version 9); the top level holds DefineShape 1-4 (tags 2, 22, 32, 83), DefineSprite (39), DefineEditText (37), DefineButton2 (34),
//! DefineMorphShape (46), ExportAssets (56), DoInitAction (59, AS2), DefineScalingGrid (78); there are NO embedded bitmaps (the pictures are the
//! external `TIF/` files).  The layouts follow the public SWF file format specification (version 19); GFx's own tags (1000+) are skipped.
//! Not done: ActionScript (only the timeline actions stop / play / gotoFrame / gotoLabel of DoAction are kept; DoInitAction and clip actions are skipped), morph
//! shapes, filters (parsed to skip them), blend modes, 9-slice scaling, bitmap fills (reported as `{"bmp": id}`), line caps and joins.
//!
//! `parse(data)` takes the whole file and returns a [`Movie`]; `Movie::to_json` is what the browser draws (twips are converted to pixels).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use serde_json::{json, Map, Value};

use crate::error::*;

struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
    bit: u32,
}

impl<'a> Bits<'a> {
    fn new(b: &'a [u8], pos: usize) -> Self {
        Bits { b, pos, bit: 0 }
    }
    fn u(&mut self, n: u32) -> Result<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self.b.get(self.pos).ok_or_else(|| Error::msg("gfx: bit data ends early"))?;
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
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}

/// Byte cursor over one tag body.
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn u8(&mut self) -> Result<u8> {
        let v = u8_at(self.b, self.p)?;
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        let v = u16_at(self.b, self.p)?;
        self.p += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32> {
        let v = u32_at(self.b, self.p)?;
        self.p += 4;
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        if self.p + n > self.b.len() {
            return Err(Error::msg("gfx: skip past the tag"));
        }
        self.p += n;
        Ok(())
    }
    fn string(&mut self) -> Result<String> {
        let rest = self.b.get(self.p..).ok_or_else(|| Error::msg("gfx: string past the tag"))?;
        let n = rest.iter().position(|&c| c == 0).ok_or_else(|| Error::msg("gfx: unterminated string"))?;
        let s = String::from_utf8_lossy(&rest[..n]).into_owned();
        self.p += n + 1;
        Ok(s)
    }
    fn rgb(&mut self, alpha: bool) -> Result<String> {
        let (r, g, b) = (self.u8()?, self.u8()?, self.u8()?);
        let a = if alpha { self.u8()? } else { 255 };
        Ok(rgba(r, g, b, a))
    }
    /// RECT as `[xmin, ymin, xmax, ymax]` in pixels.
    fn rect(&mut self) -> Result<[f64; 4]> {
        let mut r = Bits::new(self.b, self.p);
        let n = r.u(5)? as u32;
        let v = [r.s(n)?, r.s(n)?, r.s(n)?, r.s(n)?];
        r.align();
        self.p = r.pos;
        Ok([tw(v[0]), tw(v[2]), tw(v[1]), tw(v[3])])
    }
    /// MATRIX as the canvas order `[a, b, c, d, e, f]` (translation in pixels).
    fn matrix(&mut self) -> Result<[f64; 6]> {
        let mut r = Bits::new(self.b, self.p);
        let (mut a, mut d, mut b, mut c) = (1.0, 1.0, 0.0, 0.0);
        if r.u(1)? != 0 {
            let n = r.u(5)? as u32;
            a = r.s(n)? as f64 / 65536.0;
            d = r.s(n)? as f64 / 65536.0;
        }
        if r.u(1)? != 0 {
            let n = r.u(5)? as u32;
            b = r.s(n)? as f64 / 65536.0;
            c = r.s(n)? as f64 / 65536.0;
        }
        let n = r.u(5)? as u32;
        let (tx, ty) = (r.s(n)?, r.s(n)?);
        r.align();
        self.p = r.pos;
        Ok([a, b, c, d, tw(tx), tw(ty)])
    }
    /// CXFORMWITHALPHA as `[mR, mG, mB, mA, aR, aG, aB, aA]` (multipliers 1 = unchanged, adds in 0..255).
    fn cxform(&mut self, alpha: bool) -> Result<[f64; 8]> {
        let mut r = Bits::new(self.b, self.p);
        let has_add = r.u(1)? != 0;
        let has_mul = r.u(1)? != 0;
        let n = r.u(4)? as u32;
        let mut o = [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let k = if alpha { 4 } else { 3 };
        if has_mul {
            for v in o.iter_mut().take(k) {
                *v = r.s(n)? as f64 / 256.0;
            }
        }
        if has_add {
            for v in o.iter_mut().skip(4).take(k) {
                *v = r.s(n)? as f64;
            }
        }
        r.align();
        self.p = r.pos;
        Ok(o)
    }
}

fn tw(v: i64) -> f64 {
    v as f64 / 20.0
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> String {
    if a == 255 {
        format!("#{:02x}{:02x}{:02x}", r, g, b)
    } else {
        format!("rgba({},{},{},{:.3})", r, g, b, a as f64 / 255.0)
    }
}

fn num(v: f64) -> String {
    let s = format!("{:.2}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" || s == "-0" {
        "0".into()
    } else {
        s.to_string()
    }
}

// ---- shapes

#[derive(Clone, Copy)]
struct Seg {
    x0: i64,
    y0: i64,
    c: Option<(i64, i64)>,
    x1: i64,
    y1: i64,
}

impl Seg {
    fn rev(self) -> Seg {
        Seg { x0: self.x1, y0: self.y1, c: self.c, x1: self.x0, y1: self.y0 }
    }
}

enum Fill {
    Solid(String),
    Grad { radial: bool, m: [f64; 6], stops: Vec<(f64, String)> },
    Bitmap(u16),
}

impl Fill {
    fn json(&self) -> Value {
        match self {
            Fill::Solid(c) => json!({ "c": c }),
            Fill::Grad { radial, m, stops } => {
                json!({"g": if *radial { "rad" } else { "lin" }, "m": m, "s": stops.iter().map(|(r, c)| json!([r, c])).collect::<Vec<_>>()})
            }
            Fill::Bitmap(id) => json!({ "bmp": id }),
        }
    }
}

struct Line {
    w: f64,
    c: String,
}

fn fill_styles(c: &mut Cur, ver: u8) -> Result<Vec<Fill>> {
    let mut n = c.u8()? as usize;
    if n == 0xFF && ver >= 2 {
        n = c.u16()? as usize;
    }
    let mut out = vec![];
    for _ in 0..n {
        let t = c.u8()?;
        out.push(match t {
            0x00 => Fill::Solid(c.rgb(ver >= 3)?),
            0x10 | 0x12 | 0x13 => {
                let m = c.matrix()?;
                let cnt = (c.u8()? & 15) as usize;
                let mut stops = vec![];
                for _ in 0..cnt {
                    let ratio = c.u8()? as f64 / 255.0;
                    stops.push((ratio, c.rgb(ver >= 3)?));
                }
                if t == 0x13 {
                    c.skip(2)?;
                }
                Fill::Grad { radial: t != 0x10, m, stops }
            }
            0x40..=0x43 => {
                let id = c.u16()?;
                c.matrix()?;
                Fill::Bitmap(id)
            }
            _ => return Err(Error::msg(format!("gfx: fill style type {:#x}", t))),
        });
    }
    Ok(out)
}

fn line_styles(c: &mut Cur, ver: u8) -> Result<Vec<Line>> {
    let mut n = c.u8()? as usize;
    if n == 0xFF {
        n = c.u16()? as usize;
    }
    let mut out = vec![];
    for _ in 0..n {
        let w = c.u16()? as f64 / 20.0;
        if ver >= 4 {
            let flags = c.u16()?;
            if (flags >> 4) & 3 == 2 {
                c.skip(2)?; // miter limit
            }
            if flags & 0x0008 != 0 {
                let f = fill_styles_one(c, ver)?;
                out.push(Line { w, c: f });
            } else {
                out.push(Line { w, c: c.rgb(true)? });
            }
        } else {
            out.push(Line { w, c: c.rgb(ver >= 3)? });
        }
    }
    Ok(out)
}

/// The first colour of a fill style (LINESTYLE2 with a fill: drawn as that solid colour).
fn fill_styles_one(c: &mut Cur, ver: u8) -> Result<String> {
    let t = c.u8()?;
    match t {
        0x00 => c.rgb(true),
        0x10 | 0x12 | 0x13 => {
            c.matrix()?;
            let cnt = (c.u8()? & 15) as usize;
            let mut first = "#888".to_string();
            for i in 0..cnt {
                c.u8()?;
                let col = c.rgb(ver >= 3)?;
                if i == 0 {
                    first = col;
                }
            }
            if t == 0x13 {
                c.skip(2)?;
            }
            Ok(first)
        }
        _ => {
            c.u16()?;
            c.matrix()?;
            Ok("#888".into())
        }
    }
}

/// Stitch the segments of one fill into closed sub-paths: SVG-style `M..L..Q..Z` data in pixels.
fn stitch(segs: &[Seg]) -> String {
    let mut by_start: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, s) in segs.iter().enumerate() {
        by_start.entry((s.x0, s.y0)).or_default().push(i);
    }
    let mut used = vec![false; segs.len()];
    let mut out = String::new();
    for i in 0..segs.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let first = segs[i];
        out.push_str(&format!("M{} {}", num(tw(first.x0)), num(tw(first.y0))));
        let mut cur = first;
        loop {
            match cur.c {
                Some((cx, cy)) => out.push_str(&format!("Q{} {} {} {}", num(tw(cx)), num(tw(cy)), num(tw(cur.x1)), num(tw(cur.y1)))),
                None => out.push_str(&format!("L{} {}", num(tw(cur.x1)), num(tw(cur.y1)))),
            }
            if (cur.x1, cur.y1) == (first.x0, first.y0) {
                break;
            }
            let next = by_start.get(&(cur.x1, cur.y1)).and_then(|v| v.iter().copied().find(|&j| !used[j]));
            match next {
                Some(j) => {
                    used[j] = true;
                    cur = segs[j];
                }
                None => break,
            }
        }
        out.push('Z');
    }
    out
}

fn segs_path(segs: &[Seg]) -> String {
    let mut out = String::new();
    let mut at: Option<(i64, i64)> = None;
    for s in segs {
        if at != Some((s.x0, s.y0)) {
            out.push_str(&format!("M{} {}", num(tw(s.x0)), num(tw(s.y0))));
        }
        match s.c {
            Some((cx, cy)) => out.push_str(&format!("Q{} {} {} {}", num(tw(cx)), num(tw(cy)), num(tw(s.x1)), num(tw(s.y1)))),
            None => out.push_str(&format!("L{} {}", num(tw(s.x1)), num(tw(s.y1)))),
        }
        at = Some((s.x1, s.y1));
    }
    out
}

/// DefineShape / 2 / 3 / 4 body after the id and bounds: styles + records -> `{"p": [{fill, d}], "l": [{w, c, d}]}`.
fn shape_with_style(c: &mut Cur, ver: u8) -> Result<(Vec<Value>, Vec<Value>)> {
    let fills = fill_styles(c, ver)?;
    let lines = line_styles(c, ver)?;
    shape_records(c, ver, fills, lines)
}

/// The edge records of a shape whose style tables are already known (a glyph outline uses one implicit fill).
fn shape_records(c: &mut Cur, ver: u8, mut fills: Vec<Fill>, mut lines: Vec<Line>) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut r = Bits::new(c.b, c.p);
    let mut nf = r.u(4)? as u32;
    let mut nl = r.u(4)? as u32;
    let (mut x, mut y) = (0i64, 0i64);
    let (mut f0, mut f1, mut ls) = (0usize, 0usize, 0usize);
    let (mut fbase, mut lbase) = (0usize, 0usize);
    let mut fsegs: Vec<Vec<Seg>> = (0..fills.len()).map(|_| vec![]).collect();
    let mut lsegs: Vec<Vec<Seg>> = (0..lines.len()).map(|_| vec![]).collect();
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
            }
            if flags & 2 != 0 {
                f0 = r.u(nf)? as usize;
            }
            if flags & 4 != 0 {
                f1 = r.u(nf)? as usize;
            }
            if flags & 8 != 0 {
                ls = r.u(nl)? as usize;
            }
            if flags & 16 != 0 {
                r.align();
                let mut cc = Cur { b: c.b, p: r.pos };
                let nfills = fill_styles(&mut cc, ver)?;
                let nlines = line_styles(&mut cc, ver)?;
                fbase = fills.len();
                lbase = lines.len();
                fsegs.extend((0..nfills.len()).map(|_| vec![]));
                lsegs.extend((0..nlines.len()).map(|_| vec![]));
                fills.extend(nfills);
                lines.extend(nlines);
                r = Bits::new(c.b, cc.p);
                nf = r.u(4)? as u32;
                nl = r.u(4)? as u32;
                // style indices after a new table are relative to it
                if f0 != 0 {
                    f0 = 0;
                }
                if f1 != 0 {
                    f1 = 0;
                }
                ls = 0;
            }
            continue;
        }
        let seg;
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
            seg = Seg { x0: x, y0: y, c: None, x1: x + dx, y1: y + dy };
        } else {
            let n = r.u(4)? as u32 + 2;
            let (cx, cy) = (x + r.s(n)?, y + r.s(n)?);
            let (ax, ay) = (cx + r.s(n)?, cy + r.s(n)?);
            seg = Seg { x0: x, y0: y, c: Some((cx, cy)), x1: ax, y1: ay };
        }
        x = seg.x1;
        y = seg.y1;
        if f0 != 0 {
            if let Some(v) = fsegs.get_mut(fbase + f0 - 1) {
                v.push(seg.rev());
            }
        }
        if f1 != 0 {
            if let Some(v) = fsegs.get_mut(fbase + f1 - 1) {
                v.push(seg);
            }
        }
        if ls != 0 {
            if let Some(v) = lsegs.get_mut(lbase + ls - 1) {
                v.push(seg);
            }
        }
    }
    r.align();
    c.p = r.pos;
    let mut p = vec![];
    for (f, s) in fills.iter().zip(&fsegs) {
        if !s.is_empty() {
            let mut o = f.json();
            o["d"] = json!(stitch(s));
            p.push(o);
        }
    }
    let mut l = vec![];
    for (ln, s) in lines.iter().zip(&lsegs) {
        if !s.is_empty() {
            l.push(json!({"w": ln.w.max(0.5), "c": ln.c, "d": segs_path(s)}));
        }
    }
    Ok((p, l))
}

fn define_shape(body: &[u8], tag: u32) -> Result<(u16, Value)> {
    let ver = match tag {
        2 => 1,
        22 => 2,
        32 => 3,
        _ => 4,
    };
    let mut c = Cur { b: body, p: 0 };
    let id = c.u16()?;
    let b = c.rect()?;
    if ver == 4 {
        c.rect()?;
        c.u8()?;
    }
    let (p, l) = shape_with_style(&mut c, ver)?;
    Ok((id, json!({"t": "shape", "b": b, "p": p, "l": l})))
}


// ---- morph shapes

/// A morphing fill style `(start, end)`: colours and matrices interpolate with the placement's ratio.
fn morph_fill(c: &mut Cur) -> Result<Value> {
    let t = c.u8()?;
    Ok(match t {
        0x00 => json!({"c": c.rgb(true)?, "c1": c.rgb(true)?}),
        0x10 | 0x12 | 0x13 => {
            let (m0, m1) = (c.matrix()?, c.matrix()?);
            let cnt = (c.u8()? & 15) as usize;
            let mut stops = vec![];
            for _ in 0..cnt {
                let r0 = c.u8()? as f64 / 255.0;
                let c0 = c.rgb(true)?;
                let r1 = c.u8()? as f64 / 255.0;
                stops.push(json!([r0, c0, r1, c.rgb(true)?]));
            }
            if t == 0x13 {
                c.skip(4)?; // focal points (start, end)
            }
            json!({"g": if t == 0x10 { "lin" } else { "rad" }, "m": m0, "m1": m1, "s": stops})
        }
        0x40..=0x43 => {
            let id = c.u16()?;
            c.matrix()?;
            c.matrix()?;
            json!({ "bmp": id })
        }
        _ => return Err(Error::msg(format!("gfx: morph fill style type {:#x}", t))),
    })
}

/// MORPHLINESTYLE (DefineMorphShape) / MORPHLINESTYLE2 (v2): `(w0, w1, colour0, colour1)`; a line with a fill is drawn in the fill's (first) colour.
fn morph_line(c: &mut Cur, v2: bool) -> Result<(f64, f64, String, String)> {
    let (w0, w1) = (c.u16()? as f64 / 20.0, c.u16()? as f64 / 20.0);
    if !v2 {
        return Ok((w0, w1, c.rgb(true)?, c.rgb(true)?));
    }
    let flags = c.u16()?;
    if (flags >> 4) & 3 == 2 {
        c.skip(2)?; // miter limit
    }
    if flags & 0x0008 != 0 {
        let f = morph_fill(c)?;
        let col = |k: &str, k2: &str| f[k].as_str().or(f["s"][0][k2.parse::<usize>().unwrap_or(1)].as_str()).unwrap_or("#888").to_string();
        return Ok((w0, w1, col("c", "1"), col("c1", "3")));
    }
    Ok((w0, w1, c.rgb(true)?, c.rgb(true)?))
}

/// The edges of one SHAPE (records only, styles taken from the header bit counts): `(segment, fill0, fill1, line)` in file order.
fn edge_list(c: &mut Cur) -> Result<Vec<(Seg, usize, usize, usize)>> {
    let mut r = Bits::new(c.b, c.p);
    let nf = r.u(4)? as u32;
    let nl = r.u(4)? as u32;
    let (mut x, mut y) = (0i64, 0i64);
    let (mut f0, mut f1, mut ls) = (0usize, 0usize, 0usize);
    let mut out = vec![];
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
            }
            if flags & 2 != 0 {
                f0 = r.u(nf)? as usize;
            }
            if flags & 4 != 0 {
                f1 = r.u(nf)? as usize;
            }
            if flags & 8 != 0 {
                ls = r.u(nl)? as usize;
            }
            if flags & 16 != 0 {
                return Err(Error::msg("gfx: new styles inside a morph shape"));
            }
            continue;
        }
        let seg;
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
            seg = Seg { x0: x, y0: y, c: None, x1: x + dx, y1: y + dy };
        } else {
            let n = r.u(4)? as u32 + 2;
            let (cx, cy) = (x + r.s(n)?, y + r.s(n)?);
            let (ax, ay) = (cx + r.s(n)?, cy + r.s(n)?);
            seg = Seg { x0: x, y0: y, c: Some((cx, cy)), x1: ax, y1: ay };
        }
        x = seg.x1;
        y = seg.y1;
        out.push((seg, f0, f1, ls));
    }
    r.align();
    c.p = r.pos;
    Ok(out)
}

/// One quadratic segment as path text; a straight edge becomes a curve with its control point at the middle, so both ends of a morph have the same commands.
fn quad(s: &Seg) -> String {
    let (cx, cy) = s.c.unwrap_or(((s.x0 + s.x1) / 2, (s.y0 + s.y1) / 2));
    format!("Q{} {} {} {}", num(tw(cx)), num(tw(cy)), num(tw(s.x1)), num(tw(s.y1)))
}

/// `stitch` / `segs_path` for a morph: the chains are found on the start shape and the end shape follows the same order, so the two path strings
/// have identical commands (and the browser interpolates their numbers).
fn morph_path(pairs: &[(Seg, Seg)], close: bool) -> (String, String) {
    let mut by_start: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, (a, _)) in pairs.iter().enumerate() {
        by_start.entry((a.x0, a.y0)).or_default().push(i);
    }
    let mut used = vec![false; pairs.len()];
    let (mut o0, mut o1) = (String::new(), String::new());
    for i in 0..pairs.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let first = pairs[i];
        o0.push_str(&format!("M{} {}", num(tw(first.0.x0)), num(tw(first.0.y0))));
        o1.push_str(&format!("M{} {}", num(tw(first.1.x0)), num(tw(first.1.y0))));
        let mut cur = first;
        loop {
            o0.push_str(&quad(&cur.0));
            o1.push_str(&quad(&cur.1));
            if close && (cur.0.x1, cur.0.y1) == (first.0.x0, first.0.y0) {
                break;
            }
            match by_start.get(&(cur.0.x1, cur.0.y1)).and_then(|v| v.iter().copied().find(|&j| !used[j])) {
                Some(j) => {
                    used[j] = true;
                    cur = pairs[j];
                }
                None => break,
            }
        }
        if close {
            o0.push('Z');
            o1.push('Z');
        }
    }
    (o0, o1)
}

/// DefineMorphShape (46) / DefineMorphShape2 (84): `{"t": "morph", b, p: [{fill (c / c1, g / m / m1 / s, bmp), d, d1}], l: [{w, w1, c, c1, d, d1}]}`.
/// CONFIRMED against the SWF spec (v19, DEFINEMORPHSHAPE: end edges at `offset` after the offset field, edges pair one to one);
/// EMPIRICAL: all 734 + 264 definitions of the archive parse.  The shape at ratio `r` (0..65535) interpolates every number of `d` -> `d1`.
fn define_morph(body: &[u8], v2: bool) -> Result<(u16, Value)> {
    let mut c = Cur { b: body, p: 0 };
    let id = c.u16()?;
    let (b0, b1) = (c.rect()?, c.rect()?);
    if v2 {
        c.rect()?;
        c.rect()?;
        c.u8()?;
    }
    let off = c.u32()? as usize;
    let end_at = c.p + off;
    let mut nf = c.u8()? as usize;
    if nf == 0xFF {
        nf = c.u16()? as usize;
    }
    let mut fills = vec![];
    for _ in 0..nf {
        fills.push(morph_fill(&mut c)?);
    }
    let mut nl = c.u8()? as usize;
    if nl == 0xFF {
        nl = c.u16()? as usize;
    }
    let mut lines = vec![];
    for _ in 0..nl {
        lines.push(morph_line(&mut c, v2)?);
    }
    let start = edge_list(&mut c)?;
    let end = edge_list(&mut Cur { b: body, p: end_at })?;
    if start.len() != end.len() {
        return Err(Error::msg("gfx: morph start / end edge counts differ"));
    }
    let mut fp: Vec<Vec<(Seg, Seg)>> = vec![vec![]; fills.len()];
    let mut lp: Vec<Vec<(Seg, Seg)>> = vec![vec![]; lines.len()];
    for ((a, f0, f1, ls), (b, _, _, _)) in start.iter().zip(&end) {
        if *f0 != 0 {
            if let Some(v) = fp.get_mut(f0 - 1) {
                v.push((a.rev(), b.rev()));
            }
        }
        if *f1 != 0 {
            if let Some(v) = fp.get_mut(f1 - 1) {
                v.push((*a, *b));
            }
        }
        if *ls != 0 {
            if let Some(v) = lp.get_mut(ls - 1) {
                v.push((*a, *b));
            }
        }
    }
    let mut p = vec![];
    for (f, s) in fills.iter().zip(&fp) {
        if !s.is_empty() {
            let (d0, d1) = morph_path(s, true);
            let mut o = f.clone();
            o["d"] = json!(d0);
            o["d1"] = json!(d1);
            p.push(o);
        }
    }
    let mut l = vec![];
    for ((w0, w1, c0, c1), s) in lines.iter().zip(&lp) {
        if !s.is_empty() {
            let (d0, d1) = morph_path(s, false);
            l.push(json!({"w": w0.max(0.5), "w1": w1.max(0.5), "c": c0, "c1": c1, "d": d0, "d1": d1}));
        }
    }
    let b = [b0[0].min(b1[0]), b0[1].min(b1[1]), b0[2].max(b1[2]), b0[3].max(b1[3])];
    Ok((id, json!({"t": "morph", "b": b, "p": p, "l": l})))
}

// ---- timelines

fn skip_filters(c: &mut Cur) -> Result<()> {
    let n = c.u8()?;
    for _ in 0..n {
        match c.u8()? {
            0 => c.skip(23)?,
            1 => c.skip(9)?,
            2 => c.skip(15)?,
            3 => c.skip(27)?,
            4 | 7 => {
                let k = c.u8()? as usize;
                c.skip(k * 5 + 19)?;
            }
            5 => {
                let (mx, my) = (c.u8()? as usize, c.u8()? as usize);
                c.skip(8 + mx * my * 4 + 4 + 1)?;
            }
            6 => c.skip(80)?,
            t => return Err(Error::msg(format!("gfx: filter {}", t))),
        }
    }
    Ok(())
}

/// PlaceObject2 / 3 -> `["p", depth, char|null, matrix|null, cxform|null, name|null, clipDepth|null, move]`.
fn place(body: &[u8], v3: bool) -> Result<Value> {
    let mut c = Cur { b: body, p: 0 };
    let f = c.u8()?;
    let f3 = if v3 { c.u8()? } else { 0 };
    let depth = c.u16()?;
    if f3 & 0x08 != 0 || (f3 & 0x10 != 0 && f & 2 != 0) {
        c.string()?; // class name
    }
    let ch = if f & 2 != 0 { Some(c.u16()?) } else { None };
    let m = if f & 4 != 0 { Some(c.matrix()?) } else { None };
    let cx = if f & 8 != 0 { Some(c.cxform(true)?) } else { None };
    let ratio = if f & 0x10 != 0 { Some(c.u16()?) } else { None };
    let name = if f & 0x20 != 0 { Some(c.string()?) } else { None };
    let clip = if f & 0x40 != 0 { Some(c.u16()?) } else { None };
    let mut ok = true;
    if v3 && f3 & 0x01 != 0 {
        ok = skip_filters(&mut c).is_ok();
    }
    let mut v = json!(["p", depth, ch, m, cx, name, clip, f & 1 != 0, null, ratio]);
    if v3 && ok {
        for (bit, n) in [(0x02u8, 1usize), (0x04, 1), (0x20, 1), (0x40, 4)] {
            if f3 & bit != 0 {
                let _ = c.skip(n);
            }
        }
    }
    if f & 0x80 != 0 && ok {
        // CLIPACTIONS: reserved u16, all-events u32, then records {flags u32, size u32, [key code u8], actions} until flags == 0
        let mut ca: Vec<Value> = vec![];
        if c.skip(6).is_ok() {
            while let Ok(fl) = c.u32() {
                if fl == 0 {
                    break;
                }
                let Ok(size) = c.u32() else { break };
                let size = size as usize;
                let Some(rec) = c.b.get(c.p..c.p + size) else { break };
                c.p += size;
                let (key, acts) = if fl & 0x20000 != 0 && !rec.is_empty() { (rec[0], &rec[1..]) } else { (0, rec) };
                ca.push(json!([fl, key, b64(acts)]));
            }
        }
        if !ca.is_empty() {
            v[8] = Value::Array(ca);
        }
    }
    Ok(v)
}

/// Standard base64 (the action bytes travel in the JSON as text).
pub fn b64(d: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(d.len() * 4 / 3 + 4);
    for ch in d.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

/// The timeline-control actions of a DoAction body: `["stop"]`, `["play"]`, `["goto", frame0, play?]`, `["label", name, play?]`.  Everything else
/// (the real ActionScript) is skipped by its record length; `gotoAndStop(...)` written as a method call is not recognised (INFERRED common forms).
fn actions(body: &[u8]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    let mut p = 0;
    while p < body.len() {
        let op = body[p];
        p += 1;
        if op == 0 {
            break;
        }
        let (len, data) = if op >= 0x80 {
            let Ok(l) = u16_at(body, p) else { break };
            p += 2;
            let l = l as usize;
            let Some(d) = body.get(p..p + l) else { break };
            p += l;
            (l, d)
        } else {
            (0, &body[..0])
        };
        match op {
            0x07 => out.push(json!(["stop"])),
            0x06 => match out.last_mut() {
                Some(v) if v[0] == "goto" || v[0] == "label" => v[2] = json!(true),
                _ => out.push(json!(["play"])),
            },
            0x81 if len == 2 => out.push(json!(["goto", u16_at(data, 0).unwrap_or(0), false])),
            0x8C => out.push(json!(["label", String::from_utf8_lossy(&data[..data.len().saturating_sub(1)]), false])),
            _ => {}
        }
    }
    out
}

/// A tag list (the movie root or a sprite) -> frames `[{"l": label?, "o": [ops]}]`.
fn timeline(data: &[u8], start: usize, end: usize, errors: &mut u32) -> Vec<Value> {
    let mut frames = vec![];
    let mut ops: Vec<Value> = vec![];
    let mut label: Option<String> = None;
    let mut acts: Vec<Value> = vec![];
    let mut abytes: Vec<Value> = vec![];
    let mut pos = start;
    while pos + 2 <= end {
        let h = u16_at(data, pos).unwrap_or(0) as u32;
        pos += 2;
        let (t, mut ln) = (h >> 6, (h & 63) as usize);
        if ln == 63 {
            ln = u32_at(data, pos).unwrap_or(0) as usize;
            pos += 4;
        }
        if pos + ln > end {
            break;
        }
        let body = &data[pos..pos + ln];
        pos += ln;
        match t {
            0 => break,
            1 => {
                let mut f = Map::new();
                if let Some(l) = label.take() {
                    f.insert("l".into(), json!(l));
                }
                if !acts.is_empty() {
                    f.insert("a".into(), Value::Array(std::mem::take(&mut acts)));
                }
                if !abytes.is_empty() {
                    f.insert("ab".into(), Value::Array(std::mem::take(&mut abytes)));
                }
                f.insert("o".into(), Value::Array(std::mem::take(&mut ops)));
                frames.push(Value::Object(f));
            }
            4 | 26 | 70 => match place(body, t == 70) {
                Ok(v) => ops.push(v),
                Err(_) => *errors += 1,
            },
            5 | 28 => {
                let d = if t == 5 { u16_at(body, 2) } else { u16_at(body, 0) };
                if let Ok(d) = d {
                    ops.push(json!(["r", d]));
                }
            }
            12 => {
                acts.extend(actions(body));
                abytes.push(json!(b64(body)));
            }
            43 => label = Cur { b: body, p: 0 }.string().ok(),
            _ => {}
        }
    }
    if !ops.is_empty() {
        frames.push(json!({ "o": ops }));
    }
    frames
}

/// DefineFont3 (tag 75): the glyph outlines only (`"g"`: one path per glyph, in font units / 20; `"em"` = the em square in those units, 1024).
/// Static text refers to glyphs by index, so the code table, advances and kerning are not needed and are not read.  CONFIRMED against the SWF spec
/// (v19, DefineFont3: offset table relative to its own start, glyph SHAPE records with one implicit fill); EMPIRICAL: 29 fonts in fonts_en.swf parse.
fn define_font3(body: &[u8]) -> Result<(u16, Value)> {
    let mut c = Cur { b: body, p: 0 };
    let id = c.u16()?;
    let flags = c.u8()?;
    c.u8()?; // language
    let nlen = c.u8()? as usize;
    let name = String::from_utf8_lossy(&body[c.p..(c.p + nlen).min(body.len())]).trim_end_matches('\0').to_string();
    c.skip(nlen)?;
    let n = c.u16()? as usize;
    let table = c.p;
    let wide = flags & 8 != 0;
    let mut offs = Vec::with_capacity(n);
    for _ in 0..n {
        offs.push(if wide { c.u32()? as usize } else { c.u16()? as usize });
    }
    let mut glyphs = Vec::with_capacity(n);
    for o in offs {
        let mut g = Cur { b: body, p: table + o };
        let (p, _) = shape_records(&mut g, 1, vec![Fill::Solid("#000".into())], vec![])?;
        glyphs.push(p.first().and_then(|f| f["d"].as_str()).unwrap_or("").to_string());
    }
    Ok((id, json!({"t": "font", "name": name, "em": 1024, "g": glyphs})))
}

/// DefineText (11) / DefineText2 (33): glyph runs with absolute pen positions in pixels, `{"t": "stext", "b", "m", "r": [{"f": font id, "h": px, "c": colour, "g": [[glyph, x, y]]}]}`.
/// CONFIRMED against the SWF spec (v19, DEFINETEXT / TEXTRECORD); the pen position persists across records, an x / y offset sets it.
fn define_text(body: &[u8], v2: bool) -> Result<(u16, Value)> {
    let mut c = Cur { b: body, p: 0 };
    let id = c.u16()?;
    let b = c.rect()?;
    let m = c.matrix()?;
    let (gb, ab) = (c.u8()? as u32, c.u8()? as u32);
    let (mut font, mut height, mut color) = (0u16, 0.0, "#000".to_string());
    let (mut x, mut y) = (0.0f64, 0.0f64);
    let mut recs = vec![];
    loop {
        let f = c.u8()?;
        if f == 0 {
            break;
        }
        if f & 8 != 0 {
            font = c.u16()?;
        }
        if f & 4 != 0 {
            color = c.rgb(v2)?;
        }
        if f & 1 != 0 {
            x = (c.u16()? as i16) as f64 / 20.0;
        }
        if f & 2 != 0 {
            y = (c.u16()? as i16) as f64 / 20.0;
        }
        if f & 8 != 0 {
            height = c.u16()? as f64 / 20.0;
        }
        let n = c.u8()?;
        let mut r = Bits::new(c.b, c.p);
        let mut gl = vec![];
        for _ in 0..n {
            let g = r.u(gb)?;
            let a = r.s(ab)? as f64 / 20.0;
            gl.push(json!([g, num_f(x), num_f(y)]));
            x += a;
        }
        r.align();
        c.p = r.pos;
        recs.push(json!({"f": font, "h": height, "c": color, "g": gl}));
    }
    Ok((id, json!({"t": "stext", "b": b, "m": m, "r": recs})))
}

fn num_f(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn edit_text(body: &[u8]) -> Result<(u16, Value)> {
    let mut c = Cur { b: body, p: 0 };
    let id = c.u16()?;
    let b = c.rect()?;
    let (f1, f2) = (c.u8()?, c.u8()?);
    let mut font = Value::Null;
    let mut height = 12.0;
    if f1 & 1 != 0 {
        font = json!(c.u16()?);
    }
    if f2 & 0x80 != 0 {
        font = json!(c.string()?);
    }
    if f1 & 1 != 0 {
        height = c.u16()? as f64 / 20.0;
    }
    let color = if f1 & 4 != 0 { c.rgb(true)? } else { "#000".into() };
    if f1 & 2 != 0 {
        c.skip(2)?;
    }
    let mut align = 0;
    if f2 & 0x20 != 0 {
        align = c.u8()?;
        c.skip(8)?;
    }
    let var = c.string()?;
    let txt = if f1 & 0x80 != 0 { c.string()? } else { String::new() };
    Ok((id, json!({"t": "text", "b": b, "txt": txt, "var": var, "h": height, "c": color, "al": align, "ml": f1 & 0x20 != 0, "ro": f1 & 8 != 0, "html": f2 & 2 != 0, "font": font})))
}

fn button2(body: &[u8]) -> Result<(u16, Value)> {
    let mut c = Cur { b: body, p: 0 };
    let id = c.u16()?;
    c.u8()?;
    let aoff = c.u16()?;
    let mut st: [Vec<Value>; 4] = Default::default(); // up, over, down, hit
    loop {
        let f = c.u8()?;
        if f == 0 {
            break;
        }
        let ch = c.u16()?;
        let depth = c.u16()?;
        let m = c.matrix()?;
        let cx = c.cxform(true)?;
        if f & 0x10 != 0 {
            skip_filters(&mut c)?;
        }
        if f & 0x20 != 0 {
            c.u8()?;
        }
        let rec = json!([ch, depth, m, cx]);
        for (i, bit) in [0x01u8, 0x02, 0x04, 0x08].iter().enumerate() {
            if f & bit != 0 {
                st[i].push(rec.clone());
            }
        }
    }
    // BUTTONCONDACTIONs: size u16 (0 = last), condition u16, actions
    let mut ba: Vec<Value> = vec![];
    if aoff != 0 {
        let mut p = 3 + aoff as usize; // the offset counts from its own field (body offset 3)
        while let (Ok(sz), Ok(cond)) = (u16_at(body, p), u16_at(body, p + 2)) {
            let end = if sz == 0 { body.len() } else { p + sz as usize };
            let Some(a) = body.get(p + 4..end.min(body.len())) else { break };
            ba.push(json!([cond, b64(a)]));
            if sz == 0 {
                break;
            }
            p = end;
        }
    }
    let mut v = json!({"t": "button", "up": st[0], "over": st[1], "down": st[2], "hit": st[3]});
    if !ba.is_empty() {
        v["ba"] = Value::Array(ba);
    }
    Ok((id, v))
}

/// A parsed movie.
#[derive(Debug, Clone, PartialEq)]
pub struct Movie {
    pub version: u8,
    pub size: [f64; 2],
    pub fps: f64,
    pub bg: Option<String>,
    pub root: Vec<Value>,
    pub chars: BTreeMap<u16, Value>,
    pub exports: Vec<(String, u16)>,
    /// Imported symbols `(source movie, name)` (ImportAssets): drawn as empty.
    pub imports: Vec<String>,
    /// What each import provides `(source movie, local id, name in the source)`: static text finds its imported fonts through this.
    pub imap: Vec<(String, u16, String)>,
    /// DoInitAction bodies `(sprite id, bytes)`: the class and component code, run once before the sprite is first used.
    pub init: Vec<(u16, Vec<u8>)>,
    /// Tags of a type this parser does not decode, by type.
    pub skipped: BTreeMap<u32, u32>,
    /// Tags that failed to parse.
    pub errors: u32,
    /// The failed tags by type (definitions only).
    pub failed: BTreeMap<u32, u32>,
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 8 {
        return Err(Error::msg("gfx: file too short"));
    }
    match &data[..3] {
        b"CWS" | b"CFX" => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(&data[8..]).read_to_end(&mut out).map_err(|e| Error::msg(format!("gfx: zlib: {}", e)))?;
            Ok(out)
        }
        b"FWS" | b"GFX" => Ok(data[8..].to_vec()),
        s => Err(Error::msg(format!("gfx: signature {:?}", s))),
    }
}

pub fn parse(data: &[u8]) -> Result<Movie> {
    let body = decompress(data)?;
    let version = data[3];
    let mut c = Cur { b: &body, p: 0 };
    let r = c.rect()?;
    let fps = u16_at(&body, c.p)? as f64 / 256.0;
    c.p += 4; // fps (8.8) + frame count
    let mut m = Movie {
        version,
        size: [r[2] - r[0], r[3] - r[1]],
        fps,
        bg: None,
        root: vec![],
        chars: BTreeMap::new(),
        exports: vec![],
        imports: vec![],
        imap: vec![],
        init: vec![],
        skipped: BTreeMap::new(),
        errors: 0,
        failed: BTreeMap::new(),
    };
    // pass 1: definitions
    let mut pos = c.p;
    while pos + 2 <= body.len() {
        let h = u16_at(&body, pos)? as u32;
        pos += 2;
        let (t, mut ln) = (h >> 6, (h & 63) as usize);
        if ln == 63 {
            ln = u32_at(&body, pos)? as usize;
            pos += 4;
        }
        if pos + ln > body.len() {
            break;
        }
        let b = &body[pos..pos + ln];
        let at = pos;
        pos += ln;
        let res: Result<()> = (|| {
            match t {
                2 | 22 | 32 | 83 => {
                    let (id, v) = define_shape(b, t)?;
                    m.chars.insert(id, v);
                }
                46 | 84 => {
                    let (id, v) = define_morph(b, t == 84)?;
                    m.chars.insert(id, v);
                }
                11 | 33 => {
                    let (id, v) = define_text(b, t == 33)?;
                    m.chars.insert(id, v);
                }
                75 => {
                    let (id, v) = define_font3(b)?;
                    m.chars.insert(id, v);
                }
                37 => {
                    let (id, v) = edit_text(b)?;
                    m.chars.insert(id, v);
                }
                34 => {
                    let (id, v) = button2(b)?;
                    m.chars.insert(id, v);
                }
                39 => {
                    let id = u16_at(b, 0)?;
                    let n = u16_at(b, 2)?;
                    let mut errs = 0;
                    let frames = timeline(&body, at + 4, at + ln, &mut errs);
                    m.errors += errs;
                    m.chars.insert(id, json!({"t": "sprite", "n": n, "f": frames}));
                }
                56 => {
                    let mut c = Cur { b, p: 0 };
                    let n = c.u16()?;
                    for _ in 0..n {
                        let id = c.u16()?;
                        m.exports.push((c.string()?, id));
                    }
                }
                57 | 71 => {
                    let mut c = Cur { b, p: 0 };
                    let url = c.string()?;
                    if t == 71 {
                        c.skip(2)?; // reserved 1, 0
                    }
                    let n = c.u16()?;
                    for _ in 0..n {
                        let id = c.u16()?;
                        m.imap.push((url.clone(), id, c.string()?));
                    }
                    m.imports.push(url);
                }
                59 if b.len() > 2 => m.init.push((u16_at(b, 0)?, b[2..].to_vec())),
                9 => {
                    let mut c = Cur { b, p: 0 };
                    m.bg = Some(c.rgb(false)?);
                }
                0 | 1 | 4 | 5 | 12 | 26 | 28 | 43 | 59 | 69 | 70 | 77 | 78 | 74 | 88 | 82 | 72 | 76 | 18 => {}
                _ => *m.skipped.entry(t).or_default() += 1,
            }
            Ok(())
        })();
        if res.is_err() {
            m.errors += 1;
            *m.failed.entry(t).or_default() += 1;
        }
    }
    let mut errs = 0;
    m.root = timeline(&body, c.p, body.len(), &mut errs);
    m.errors += errs;
    Ok(m)
}

impl Movie {
    pub fn to_json(&self) -> Value {
        let chars: Map<String, Value> = self.chars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let exports: Map<String, Value> = self.exports.iter().map(|(n, id)| (n.clone(), json!(id))).collect();
        json!({
            "version": self.version, "size": self.size, "fps": self.fps, "bg": self.bg,
            "root": self.root, "chars": chars, "exports": exports, "imports": self.imports, "imap": self.imap.iter().map(|(u, i, n)| json!([u, i, n])).collect::<Vec<_>>(), "init": self.init.iter().map(|(i, b)| json!([i, b64(b)])).collect::<Vec<_>>(),
            "skipped": self.skipped.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(), "errors": self.errors, "failed": self.failed.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built FWS movie: a 100x50 px stage, a red square shape (id 1) placed on frame 1, then an export of it.
    fn movie() -> Vec<u8> {
        fn tag(t: u32, b: &[u8]) -> Vec<u8> {
            let mut o = vec![];
            o.extend(((t << 6) | 63).to_le_bytes()[..2].to_vec());
            o.extend((b.len() as u32).to_le_bytes());
            o.extend(b);
            o
        }
        // RECT 0..2000 x 0..1000 twips: nbits 12 (signed 12 bit holds 1000 but not 2000 -> use 13)
        let mut bits = String::new();
        let nb = 13;
        bits += &format!("{:05b}", nb);
        for v in [0i32, 2000, 0, 1000] {
            bits += &format!("{:013b}", v);
        }
        let mut rect = vec![];
        while bits.len() % 8 != 0 {
            bits.push('0');
        }
        for i in (0..bits.len()).step_by(8) {
            rect.push(u8::from_str_radix(&bits[i..i + 8], 2).unwrap());
        }
        let mut body = rect.clone();
        body.extend([0, 12, 1, 0]); // fps 12.0, 1 frame
        // DefineShape id 1, bounds 0..200 (nbits 9), one solid red fill, 4 straight edges (a 10x10 px square)
        let mut s = vec![1, 0];
        let mut b = String::from("01001");
        for v in [0i32, 200, 0, 200] {
            b += &format!("{:09b}", v & 0x1ff);
        }
        while b.len() % 8 != 0 {
            b.push('0');
        }
        for i in (0..b.len()).step_by(8) {
            s.push(u8::from_str_radix(&b[i..i + 8], 2).unwrap());
        }
        s.extend([1, 0, 255, 0, 0]); // 1 fill: solid RGB red
        s.push(0); // 0 line styles
        let mut r = String::from("00010001"); // 1 fill bit, 1 line bit
        // style change: moveTo + fill1: 0 00 flags(5)=0b00101 -> hasMove(1) fill1(4)
        r += "0";
        r += "00101";
        r += "01000"; // moveTo nbits=8
        r += &format!("{:08b}{:08b}", 0, 0);
        r += "1"; // fill1 = style 1
        for (dx, dy) in [(200i32, 0i32), (0, 200), (-200, 0), (0, -200)] {
            r += "11"; // straight edge
            r += "1000"; // nbits = 10 (value 8 + 2)
            if dx != 0 && dy != 0 {
                unreachable!();
            }
            if dx != 0 {
                r += "0";
                r += "0";
                r += &format!("{:010b}", dx & 0x3ff);
            } else {
                r += "0";
                r += "1";
                r += &format!("{:010b}", dy & 0x3ff);
            }
        }
        r += "000000"; // end record
        while r.len() % 8 != 0 {
            r.push('0');
        }
        for i in (0..r.len()).step_by(8) {
            s.push(u8::from_str_radix(&r[i..i + 8], 2).unwrap());
        }
        body.extend(tag(2, &s));
        // PlaceObject2 depth 1, char 1, matrix identity
        body.extend(tag(26, &[0x06, 1, 0, 1, 0, 0b0000_0000]));
        body.extend(tag(1, &[]));
        // ExportAssets: 1 symbol "Box" = char 1
        body.extend(tag(56, &[1, 0, 1, 0, b'B', b'o', b'x', 0]));
        body.extend(tag(0, &[]));
        let mut f = b"FWS\x09".to_vec();
        f.extend(((body.len() + 8) as u32).to_le_bytes());
        f.extend(body);
        f
    }

    #[test]
    fn parses_a_shape_placement_and_export() {
        let m = parse(&movie()).unwrap();
        assert_eq!(m.errors, 0);
        assert_eq!(m.size, [100.0, 50.0]);
        assert_eq!(m.exports, vec![("Box".to_string(), 1)]);
        let s = &m.chars[&1];
        assert_eq!(s["t"], "shape");
        assert_eq!(s["p"][0]["c"], "#ff0000");
        assert_eq!(s["p"][0]["d"], "M0 0L10 0L10 10L0 10L0 0Z");
        assert_eq!(m.root.len(), 1);
        assert_eq!(m.root[0]["o"][0][0], "p");
        assert_eq!(m.root[0]["o"][0][2], 1);
    }

    #[test]
    fn keeps_the_timeline_actions() {
        // stop; gotoFrame 3 + play; gotoLabel "x" (no play); an ActionPush (0x96) is skipped by its length
        let mut b = vec![0x07, 0x81, 2, 0, 3, 0, 0x06, 0x8C, 2, 0, b'x', 0, 0x96, 3, 0, 0, b'a', 0, 0];
        assert_eq!(actions(&b), vec![json!(["stop"]), json!(["goto", 3, true]), json!(["label", "x", false])]);
        b.truncate(2); // cut inside a record: no panic
        assert_eq!(actions(&b), vec![json!(["stop"])]);
    }

    #[test]
    fn base64_pads() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"nope nope").is_err());
        assert!(parse(b"FWS\x09").is_err());
    }

    #[test]
    fn stitching_joins_reversed_segments() {
        let a = Seg { x0: 0, y0: 0, c: None, x1: 20, y1: 0 };
        let b = Seg { x0: 20, y0: 20, c: None, x1: 20, y1: 0 }.rev();
        let c = Seg { x0: 20, y0: 20, c: None, x1: 0, y1: 0 };
        assert_eq!(stitch(&[a, b, c]), "M0 0L1 0L1 1L0 0Z");
    }

    fn bytes_of(mut bits: String) -> Vec<u8> {
        while bits.len() % 8 != 0 {
            bits.push('0');
        }
        (0..bits.len()).step_by(8).map(|i| u8::from_str_radix(&bits[i..i + 8], 2).unwrap()).collect()
    }

    #[test]
    fn reads_a_font_and_static_text() {
        // font 5 "F": one glyph, a 10x10 square (200 twips) built like the shape in `movie()`
        let mut r = String::from("00010000"); // 1 fill bit, 0 line bits
        r += "000101"; // style change, moveTo + fill1
        r += "01000";
        r += &format!("{:08b}{:08b}", 0, 0);
        r += "1";
        for (dx, dy) in [(200i32, 0i32), (0, 200), (-200, 0), (0, -200)] {
            r += "111000";
            r += if dx != 0 { "00" } else { "01" };
            r += &format!("{:010b}", (if dx != 0 { dx } else { dy }) & 0x3ff);
        }
        r += "000000";
        let mut font = vec![5, 0, 0, 0, 1, b'F', 1, 0];
        font.extend([4, 0]); // offset table: the glyph follows the table (2 bytes) and the code table offset (2 bytes)
        font.extend([0, 0]); // code table offset (unused)
        font.extend(bytes_of(r));
        let (id, f) = define_font3(&font).unwrap();
        assert_eq!(id, 5);
        assert_eq!(f["name"], "F");
        assert_eq!(f["g"][0], "M0 0L10 0L10 10L0 10L0 0Z");

        // text 6: empty bounds, identity matrix, font 5 at 10 px, red, pen (1, 10), glyphs 0 (advance 5 px) and 0
        let mut t = vec![6, 0, 0, 0b0000_0000, 1, 8];
        t.extend([0x8F, 5, 0, 255, 0, 0, 20, 0, 200, 0, 200, 0, 2]);
        t.extend(bytes_of("0011001000000000".to_string() + "0"));
        t.push(0);
        let (id, v) = define_text(&t, false).unwrap();
        assert_eq!(id, 6);
        let rec = &v["r"][0];
        assert_eq!((rec["f"].as_u64(), rec["h"].as_f64(), rec["c"].as_str()), (Some(5), Some(10.0), Some("#ff0000")));
        assert_eq!(rec["g"], json!([[0, 1.0, 10.0], [0, 6.0, 10.0]]));
    }

    #[test]
    fn reads_a_morph_shape_and_the_placement_ratio() {
        fn square(side: i32) -> String {
            let mut r = String::new();
            for (dx, dy) in [(side, 0i32), (0, side), (-side, 0), (0, -side)] {
                r += "111000";
                r += if dx != 0 { "00" } else { "01" };
                r += &format!("{:010b}", (if dx != 0 { dx } else { dy }) & 0x3ff);
            }
            r + "000000"
        }
        // start: 1 fill bit; move to 0,0 + fill1; end: no style bits, move only
        let start = bytes_of(String::from("00010000") + "000101" + "01000" + &format!("{:08b}{:08b}", 0, 0) + "1" + &square(200));
        let end = bytes_of(String::from("00000000") + "000001" + "01000" + &format!("{:08b}{:08b}", 0, 0) + &square(400));
        let mut body = vec![7, 0, 0, 0]; // id, start bounds, end bounds (empty rects, one byte each)
        let mut styles = vec![1u8, 0, 255, 0, 0, 255, 0, 0, 255, 255]; // 1 solid fill red -> blue
        styles.push(0); // no line styles
        body.extend(((styles.len() + start.len()) as u32).to_le_bytes());
        body.extend(styles);
        body.extend(start);
        body.extend(end);
        let (id, v) = define_morph(&body, false).unwrap();
        assert_eq!(id, 7);
        assert_eq!((v["p"][0]["c"].as_str(), v["p"][0]["c1"].as_str()), (Some("#ff0000"), Some("#0000ff")));
        let (d0, d1) = (v["p"][0]["d"].as_str().unwrap(), v["p"][0]["d1"].as_str().unwrap());
        assert_eq!(d0, "M0 0Q5 0 10 0Q10 5 10 10Q5 10 0 10Q0 5 0 0Z");
        assert_eq!(d1, "M0 0Q10 0 20 0Q20 10 20 20Q10 20 0 20Q0 10 0 0Z");
        // PlaceObject2 depth 1, char 7, ratio 0x8000
        let p = place(&[0x16, 1, 0, 7, 0, 0b0000_0000, 0, 0x80], false).unwrap();
        assert_eq!(p[9], 0x8000);
    }
}
