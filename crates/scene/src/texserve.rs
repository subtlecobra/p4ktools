//! Texture serving for the viewers.
//!
//! The registry (`materials::MatRegistry`, lazy mode) lists textures as `{path, chan, wear}`; the web layer asks for one of them with
//! the families of compressed formats the browser has (`accept`: "s3tc", "bptc") and a size.  What is served:
//!
//! * raw BC1/2/3 (s3tc) and BC7 (bptc) mips, when the browser takes them undecoded: `u32 LE header length`, the header JSON
//!   `{"fmt","w","h","mips":[[w,h,size],...]}` padded with spaces to a multiple of 4, then the mips largest first down to the last one
//!   whose sides are multiples of 4 (`image/octet-stream`);
//! * otherwise a PNG of the largest available mip whose sides are <= min(size, 512) (the cap was a decoding-speed limit, kept for
//!   compatibility with the cache keys; decoding is fast here, but the viewer's own quality slider and the PNG fallback cap of 512 are
//!   part of the contract);
//! * `chan` "ng" (ddna normal + gloss packed), "hw" (hue/AO/luminance + wear map in the alpha), "ao" (macro AO map as hue/AO/luminance),
//!   "or" (HairPBR opacity as alpha), "a" (the alpha / gloss companion file `.dds.a`).
//!
//! `size` is snapped to the nearest of `TEX_SIZES` (64 .. 4096) so the cache stays small.  `TextureHost` keeps an in-memory
//! cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use p4k_formats::dds;
use p4k_formats::png::png_rgba;
use p4k::P4k;

use crate::textures::{level_rgba, ng_png, opacity_png, texture_base, DdsTexture};

/// largest mip (px) sent when the viewer does not ask for a size
pub const TEX_DEFAULT: usize = 1024;
/// decoded PNGs never go above this, whatever size is asked
pub const TEX_PNG_MAX: usize = 512;
pub const TEX_SIZES: [usize; 7] = [64, 128, 256, 512, 1024, 2048, 4096];
/// normal / gloss maps are decoded too (BC5S ...) and use the same cap
pub const TEX_DATA_PNG_MAX: usize = 512;
pub const HUE_LUT: &str = "Engine/EngineAssets/Textures/layerblendHueLUT.dds";

/// The family of compressed formats the GPU can take undecoded ("s3tc" / "bptc"), if the format has one.
pub fn raw_family(fmt: &str) -> Option<&'static str> {
    match fmt {
        "bc1" | "bc2" | "bc3" => Some("s3tc"),
        "bc7" => Some("bptc"),
        _ => None,
    }
}

/// The nearest known size (ties go to the smaller).
pub fn snap_size(size: usize) -> usize {
    *TEX_SIZES.iter().min_by_key(|&&s| (s as i64 - size as i64).abs()).unwrap()
}

/// A registry texture entry: `{path, chan, wear}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TexRef {
    pub path: String,
    pub chan: String,
    pub wear: String,
}

impl TexRef {
    pub fn new(path: &str, chan: &str, wear: &str) -> Self {
        TexRef { path: path.into(), chan: chan.into(), wear: wear.into() }
    }

    /// From a `textures[i]` object of the lazy registry export.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        v.get("path")?;
        Some(TexRef { path: g("path"), chan: g("chan"), wear: g("wear") })
    }
}

/// (content type, bytes)
pub type Served = (&'static str, Arc<Vec<u8>>);

fn png(w: usize, h: usize, px: &[u8]) -> Served {
    ("image/png", Arc::new(png_rgba(w as u32, h as u32, px)))
}

fn open<'a>(p4k: &'a P4k, base: &str) -> Option<DdsTexture<'a>> {
    let ent = p4k.find_path(&format!("{}.dds", base), ".dds")?;
    DdsTexture::new(p4k, ent, false).ok()
}

/// A HardSurface macro AO map as a hue/AO/luminance image: (hue 0.5, AO = the map's red channel, luminance 0.5), so the layered
/// shader's existing AO multiply applies it and the neutral hue / luminance leave the colours alone.  One PNG.
pub fn build_ao(p4k: &P4k, path: &str, size: usize) -> Option<Served> {
    if path.is_empty() {
        return None;
    }
    let t = open(p4k, &texture_base(path))?;
    let (_, w, h, rgba) = level_rgba(&t, size.min(512))?;
    let mut out = [128u8, 0, 128, 255].repeat(w * h);
    for i in 0..w * h {
        out[i * 4 + 1] = rgba[i * 4];
    }
    Some(png(w, h, &out))
}

/// Hue/AO/luminance image (RGB) with the wear map's red channel in alpha, as one PNG.  A missing hal map becomes the neutral (hue 0.5,
/// AO 1, luminance 0.5); a missing wear map becomes alpha 255.  A wear map of another size is read nearest neighbour.
pub fn build_hw(p4k: &P4k, hal: &str, wear: &str, size: usize) -> Option<Served> {
    let level = |path: &str, cap: usize| -> Option<(usize, usize, Vec<u8>)> {
        if path.is_empty() {
            return None;
        }
        let t = open(p4k, &texture_base(path))?;
        let (_, w, h, px) = level_rgba(&t, cap)?;
        Some((w, h, px))
    };
    let cap = size.min(512);
    let (a, b) = (level(hal, cap), level(wear, cap));
    if a.is_none() && b.is_none() {
        return None;
    }
    let (w, h) = a.as_ref().or(b.as_ref()).map(|x| (x.0, x.1)).unwrap();
    let mut out = match &a {
        None => [128u8, 255, 128, 255].repeat(w * h),
        Some(a) => a.2.clone(),
    };
    match &b {
        Some(b) if (b.0, b.1) == (w, h) => {
            for i in 0..w * h {
                out[i * 4 + 3] = b.2[i * 4];
            }
        }
        Some(b) => {
            for y in 0..h {
                let sy = y * b.1 / h;
                for x in 0..w {
                    out[(y * w + x) * 4 + 3] = b.2[(sy * b.0 + x * b.0 / w) * 4];
                }
            }
        }
        None => {
            for i in 0..w * h {
                out[i * 4 + 3] = 255;
            }
        }
    }
    Some(png(w, h, &out))
}

/// The raw-mips reply for a texture the browser takes undecoded (see the module doc), or None.
fn raw_reply(tex: &DdsTexture, size: usize) -> Option<Served> {
    let mut first = (0..tex.mips).find(|&i| tex.dims[i].0.max(tex.dims[i].1) <= size).unwrap_or(tex.mips - 1);
    let mut levels: Vec<(usize, usize, Vec<u8>)> = vec![];
    for i in first.min(tex.mips)..tex.mips {
        let (w, h) = tex.dims[i];
        if w % 4 != 0 || h % 4 != 0 {
            break;
        }
        match tex.level(i) {
            Some(d) => levels.push((w, h, d)),
            None => {
                if i == first {
                    // the first wanted level is absent: fall back to the next smaller one
                    first += 1;
                    continue;
                }
                break;
            }
        }
    }
    if levels.is_empty() {
        return None;
    }
    let mips: Vec<String> = levels.iter().map(|(w, h, d)| format!("[{},{},{}]", w, h, d.len())).collect();
    let mut head = format!("{{\"fmt\":\"{}\",\"w\":{},\"h\":{},\"mips\":[{}]}}", tex.format.name, levels[0].0, levels[0].1, mips.join(",")).into_bytes();
    head.resize(head.len() + (4 - head.len() % 4) % 4, b' ');
    let mut out = (head.len() as u32).to_le_bytes().to_vec();
    out.extend(head);
    for (_, _, d) in &levels {
        out.extend_from_slice(d);
    }
    Some(("application/octet-stream", Arc::new(out)))
}

/// A sprite texture as one RGBA PNG: the colour map with its separate `.dds.a` companion (the BC4 alpha plane the engine keeps beside
/// `_dif` maps, EMPIRICAL: the smoke sprites' colour map has no alpha of its own) written into alpha.  None when the colour map is not a
/// decoded format (BC7 sprites carry their alpha inline; with bptc they go to the browser raw through `build_texture`, otherwise this decodes them too, the `.dds.a` overriding).
pub fn build_sprite(p4k: &P4k, path: &str, size: usize) -> Option<Served> {
    let base = texture_base(path);
    let t = open(p4k, &base)?;
    let (lvl, w, h, mut px) = level_rgba(&t, size.min(TEX_PNG_MAX))?;
    let alpha = p4k.find_path(&format!("{}.dds.a", base), ".a").and_then(|g| DdsTexture::new(p4k, g, true).ok());
    if let Some(a) = alpha.as_ref().filter(|g| dds::DECODABLE.contains(&g.format.name) && lvl < g.mips && g.dims[lvl] == (w, h)) {
        let ad = dds::decode(a.format.name, &a.level(lvl)?, w, h).ok()?;
        for i in 0..w * h {
            px[i * 4 + 3] = ad[i * 4];
        }
    }
    Some(png(w, h, &px))
}

/// Serves one registry texture (including the "or" channel).  `accept`: the compressed
/// families the browser has.  `size` must already be snapped (`snap_size`).
pub fn build_texture(p4k: &P4k, e: &TexRef, accept: &[&str], size: usize) -> Option<Served> {
    let base = if e.path.is_empty() { String::new() } else { texture_base(&e.path) };
    match e.chan.as_str() {
        "ng" => return ng_png(p4k, &base, size.min(TEX_DATA_PNG_MAX)).map(|g| ("image/png", Arc::new(g.2))),
        "or" => return opacity_png(p4k, &base, size.min(TEX_PNG_MAX)).map(|g| ("image/png", Arc::new(g.2))),
        "hw" => return build_hw(p4k, &e.path, &e.wear, size),
        "ao" => return build_ao(p4k, &e.path, size),
        _ => {}
    }
    let alpha = e.chan == "a";
    let ent = p4k.find_path(&format!("{}{}", base, if alpha { ".dds.a" } else { ".dds" }), if alpha { ".a" } else { ".dds" })?;
    let tex = DdsTexture::new(p4k, ent, alpha).ok()?;
    let fmt = tex.format.name;
    if raw_family(fmt).map_or(false, |f| accept.contains(&f)) {
        return raw_reply(&tex, size);
    }
    if dds::DECODABLE.contains(&fmt) {
        let cap = if matches!(fmt, "bc4" | "bc4s" | "bc5" | "bc5s") { TEX_DATA_PNG_MAX } else { TEX_PNG_MAX };
        let (_, w, h, px) = level_rgba(&tex, size.min(cap))?;
        return Some(png(w, h, &px));
    }
    None
}

/// The layered shader's hue table as served bytes: u32 width, height, depth, then RGB bytes (z, y, x order).
///
/// EMPIRICAL, and CONFIRMED against the LayerBlend pixel shader: an 8 x 8 x 16 volume, 24-bit BGR in the file.  The shader samples it at (r',
/// g', 0.5 * (b' + (hue < 0 ? 1 : 0))) with c' = clamp(c * (0.8 + 0.4 lum)) * 0.875 + 0.0625, so the depth holds two 8-step colour
/// cubes, one for positive and one for negative hue shifts.
pub fn hue_lut(p4k: &P4k) -> Option<Vec<u8>> {
    let d = p4k.read(p4k.get(HUE_LUT)?).ok()?;
    let u = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    // header: height at +12, width at +16, depth at +24 (volume DDS); 24-bit pixels, masks R 0xff0000 G 0xff00 B 0xff
    let (h, w, depth) = (u(12)?, u(16)?, u(24)?);
    let n = w * h * depth * 3;
    let pix = d.get(128..128 + n)?;
    let mut out = Vec::with_capacity(12 + n);
    for v in [w, h, depth] {
        out.extend((v as u32).to_le_bytes());
    }
    for px in pix.chunks_exact(3) {
        out.extend([px[2], px[1], px[0]]);
    }
    Some(out)
}

/// `texture(path, chan, wear, accept, size)` with a small in-memory cache keyed by all those
/// parts.  Thread-safe: concurrent requests build concurrently, a repeated request is served from the cache.
pub struct TextureHost<'a> {
    p4k: &'a P4k,
    cache: Mutex<HashMap<(TexRef, Vec<String>, usize), Served>>,
    lut: Mutex<Option<Option<Vec<u8>>>>,
}

impl<'a> TextureHost<'a> {
    pub const CACHE_MAX: usize = 256;

    pub fn new(p4k: &'a P4k) -> Self {
        TextureHost { p4k, cache: Mutex::new(HashMap::new()), lut: Mutex::new(None) }
    }

    pub fn texture(&self, e: &TexRef, accept: &[&str], size: usize) -> Option<Served> {
        let size = snap_size(size);
        let mut acc: Vec<String> = accept.iter().filter(|a| **a == "s3tc" || **a == "bptc").map(|a| a.to_string()).collect();
        acc.sort();
        acc.dedup();
        let mut entry = e.clone();
        if entry.chan != "hw" {
            entry.wear.clear(); // only the hw channel has a wear map
        }
        let key = (entry.clone(), acc.clone(), size);
        if let Some(h) = self.cache.lock().unwrap().get(&key) {
            return Some(h.clone());
        }
        let acc_ref: Vec<&str> = acc.iter().map(String::as_str).collect();
        let got = build_texture(self.p4k, &entry, &acc_ref, size)?; // a miss is not cached
        let mut c = self.cache.lock().unwrap();
        if c.len() >= Self::CACHE_MAX {
            if let Some(k) = c.keys().next().cloned() {
                c.remove(&k);
            }
        }
        c.insert(key, got.clone());
        Some(got)
    }

    /// The layered shader's hue table (cached), or None.
    pub fn lut(&self) -> Option<Vec<u8>> {
        let mut l = self.lut.lock().unwrap();
        l.get_or_insert_with(|| hue_lut(self.p4k)).clone().filter(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_snap_to_the_nearest_known_one() {
        assert_eq!(snap_size(1000), 1024);
        assert_eq!(snap_size(96), 64); // tie: the smaller
        assert_eq!(snap_size(5000), 4096);
        assert_eq!(snap_size(1), 64);
        assert_eq!(snap_size(200), 256);
    }

    #[test]
    fn families() {
        assert_eq!(raw_family("bc1"), Some("s3tc"));
        assert_eq!(raw_family("bc7"), Some("bptc"));
        assert_eq!(raw_family("bc5"), None);
    }

    #[test]
    fn texref_from_registry_entry() {
        let v = serde_json::json!({"path": "a/b.tif", "chan": "hw", "wear": "c.tif"});
        assert_eq!(TexRef::from_json(&v), Some(TexRef::new("a/b.tif", "hw", "c.tif")));
        assert_eq!(TexRef::from_json(&serde_json::json!({})), None);
    }
}
