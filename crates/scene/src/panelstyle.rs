//! Manufacturer accent colour of a door / elevator panel display, taken from the game's own UI assets (no per-ship table).
//!
//! EVIDENCE (archive, 2026-10).  CONFIRMED: the door panel UI kit ships per-manufacturer vector parts,
//! `UI/Textures/Vector/{Aegis,RSI,Generic}/Doors/` (button background / border / decoration / overlay SVGs); their dominant fills are Aegis
//! `#6CB8C7`, RSI `#F19220`, Generic `#3B9EB7`, and every kit uses green for the "available" decoration and red for the overlay.  Drake has a
//! dedicated panel background (`ModularKitStyles/DRAK/Drake_DoorPanel_Background-DarkTheme.dds`).
//! EMPIRICAL / INFERRED: for the other makers the accent is the saturation-weighted mean colour of their `ModularKitStyles/<code>*/` background
//! texture (INFERRED that a maker's environment-screen kit colour is also its door panel colour).  A maker without any such asset gets `""` and
//! the page falls back to the MFD theme accent or the generic cyan.

use std::collections::HashMap;
use std::sync::Mutex;

use p4k_formats::dds;
use p4k::P4k;
use regex::Regex;

use crate::mfd;
use crate::textures::DdsTexture;

const KIT: &str = "data/ui/textures/modularkitstyles/";

/// RGB to HSV, all in 0..1 (the same formula as Python's `colorsys.rgb_to_hsv`).
pub fn rgb_to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = r.max(g).max(b);
    let minc = r.min(g).min(b);
    let rangec = maxc - minc;
    let v = maxc;
    if minc == maxc {
        return (0.0, 0.0, v);
    }
    let s = rangec / maxc;
    let rc = (maxc - r) / rangec;
    let gc = (maxc - g) / rangec;
    let bc = (maxc - b) / rangec;
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    ((h / 6.0).rem_euclid(1.0), s, v)
}

/// HSV to RGB, all in 0..1 (the same formula as Python's `colorsys.hsv_to_rgb`).
pub fn hsv_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (v, v, v);
    }
    let i = (h * 6.0) as i64;
    let f = (h * 6.0) - i as f64;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

fn hex6(h: &str) -> (f64, f64, f64) {
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f64 / 255.0;
    (c(0), c(2), c(4))
}

/// Most common saturated fill of an SVG (`#rrggbb`), ignoring white / grey; "" when there is none.  Ties go to the colour seen first.
pub fn svg_accent(text: &str) -> String {
    let re = Regex::new(r##"fill="#([0-9A-Fa-f]{6})""##).unwrap();
    let mut order: Vec<String> = vec![];
    let mut count: HashMap<String, usize> = HashMap::new();
    for c in re.captures_iter(text) {
        let h = &c[1];
        let (r, g, b) = hex6(h);
        if rgb_to_hsv(r, g, b).1 > 0.25 {
            let k = format!("#{}", h.to_lowercase());
            let e = count.entry(k.clone()).or_insert(0);
            if *e == 0 {
                order.push(k);
            }
            *e += 1;
        }
    }
    let mut best: Option<&String> = None;
    for k in &order {
        if best.map_or(true, |b| count[k] > count[b]) {
            best = Some(k);
        }
    }
    best.cloned().unwrap_or_default()
}

/// Saturation-weighted mean colour (`#rrggbb`) of RGBA bytes, sampling every `step`th pixel; "" for an image with no colour.
pub fn pixels_accent(rgba: &[u8], step: usize) -> String {
    let (mut sr, mut sg, mut sb, mut sw) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut i = 0;
    while i + 3 < rgba.len() {
        let (r, g, b) = (rgba[i] as f64 / 255.0, rgba[i + 1] as f64 / 255.0, rgba[i + 2] as f64 / 255.0);
        let (_, s, v) = rgb_to_hsv(r, g, b);
        let w = s * v * v;
        sr += r * w;
        sg += g * w;
        sb += b * w;
        sw += w;
        i += 4 * step;
    }
    if sw <= 1e-6 {
        return String::new();
    }
    let c = |x: f64| ((x / sw * 255.0 * 1.0) as i64).min(255);
    format!("#{:02x}{:02x}{:02x}", c(sr), c(sg), c(sb))
}

/// Raise the value of a dark accent so it reads as text / lines on a dark display (hue and saturation kept).
pub fn brighten(hexcol: &str, vmin: f64) -> String {
    let (r, g, b) = hex6(&hexcol[1..]);
    let (h, s, v) = rgb_to_hsv(r, g, b);
    let (r, g, b) = hsv_to_rgb(h, s, v.max(vmin));
    format!("#{:02x}{:02x}{:02x}", (r * 255.0) as i64, (g * 255.0) as i64, (b * 255.0) as i64)
}

/// Background textures of the manufacturer's ModularKitStyles folders (`CODE` and `CODE_ENV`), door panel one first, ENV next.
fn kit_files(p4k: &P4k, code: &str) -> Vec<String> {
    let c = code.to_lowercase();
    let mut out: Vec<String> = vec![];
    for e in &p4k.entries {
        let low = e.name.to_lowercase();
        if low.starts_with(KIT) && low.ends_with(".dds") {
            let folder = low[KIT.len()..].split('/').next().unwrap_or("").to_string();
            if folder == c || folder == format!("{}_env", c) {
                out.push(e.name.to_string());
            }
        }
    }
    out.sort_by_cached_key(|n| {
        let l = n.to_lowercase();
        (!l.contains("doorpanel"), !l.contains("_env"), !l.contains("bg") && !l.contains("background"), n.clone())
    });
    out
}

/// Accent colour for a four-letter manufacturer code from the archive assets above, or "".  Reads at most a few small files.
pub fn maker_accent(p4k: &P4k, code: &str) -> String {
    let vec = match code {
        "AEGS" => Some("aegis"),
        "RSI" => Some("rsi"),
        _ => None,
    };
    if let Some(vec) = vec {
        let pre = format!("data/ui/textures/vector/{}/doors/", vec);
        let parts: Vec<&p4k::Entry> = p4k
            .entries
            .iter()
            .filter(|e| {
                let l = e.name.to_lowercase();
                l.starts_with(&pre) && l.ends_with(".svg")
            })
            .collect();
        let bg: Vec<&p4k::Entry> = parts.iter().copied().filter(|e| e.name.to_lowercase().contains("background")).collect();
        for sel in [bg, parts] {
            let text: String = sel.iter().map(|e| p4k.read(e).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()).collect();
            let a = svg_accent(&text);
            if !a.is_empty() {
                return a;
            }
        }
    }
    for name in kit_files(p4k, code).iter().take(3) {
        let Some(ent) = p4k.get(name) else { continue };
        let Ok(t) = DdsTexture::new(p4k, ent, false) else { continue };
        if !["bc1", "bc2", "bc3"].contains(&t.format.name) || t.mips != 1 || t.first_stored != 0 {
            continue;
        }
        let Some(level) = t.level(0) else { continue };
        let Ok(px) = dds::decode(t.format.name, &level, t.width, t.height) else { continue };
        let a = pixels_accent(&px, 7);
        if !a.is_empty() {
            return brighten(&a, 0.75);
        }
    }
    String::new()
}

/// Per-archive cache of manufacturer accents; `of(hints)` -> (code, accent).
pub struct PanelStyle<'a> {
    p4k: &'a P4k,
    acc: Mutex<HashMap<String, String>>,
}

impl<'a> PanelStyle<'a> {
    pub fn new(p4k: &'a P4k) -> Self {
        PanelStyle { p4k, acc: Mutex::new(HashMap::new()) }
    }

    /// (manufacturer code, accent hex) for the first hint (entity class, geometry path, hull path, ship title) that names a known maker.
    pub fn of(&self, hints: &[&str]) -> (String, String) {
        for h in hints {
            let h = h.rsplit(':').next().unwrap_or("").rsplit(' ').next().unwrap_or("");
            let code = mfd::manufacturer(h, h);
            if !code.is_empty() {
                let mut acc = self.acc.lock().unwrap();
                let a = acc.entry(code.clone()).or_insert_with(|| maker_accent(self.p4k, &code)).clone();
                return (code, a);
            }
        }
        (String::new(), String::new())
    }
}
