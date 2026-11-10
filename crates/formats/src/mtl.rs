//! Material files (.mtl, CryXmlB).
//!
//! Root `<Material>` with `<SubMaterials>`, or a lone material.  Colours are "r,g,b" floats, textures are
//! `<Texture Map="TexSlot1" File="...tif"/>` (TexSlot1 = diffuse map); Shader="NoDraw" marks collision-proxy faces.

use crate::cryxml::{self, Element};
use crate::error::*;

/// An attribute string as a float (surrounding whitespace allowed); None when it does not parse.
pub fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Some(f64::INFINITY),
        "-inf" | "-infinity" => return Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Some(f64::NAN),
        _ => {}
    }
    if t.is_empty() || t.contains(|c: char| c.is_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    t.parse::<f64>().ok()
}

fn floats<const N: usize>(s: Option<&str>, default: [f64; N]) -> [f64; N] {
    let Some(s) = s else { return default };
    let v: Option<Vec<f64>> = s.split(',').map(py_float).collect();
    match v {
        Some(v) if v.len() == N => {
            let mut out = [0.0; N];
            out.copy_from_slice(&v);
            out
        }
        _ => default,
    }
}

fn float(s: Option<&str>, default: f64) -> f64 {
    s.and_then(py_float).unwrap_or(default)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rot {
    pub rtype: i64,
    pub w: f64,
    pub rate: f64,
    pub c: (f64, f64),
}

/// The non-identity parts of a slot's `<TexMod>`; see `texmod`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TexMod {
    pub tile: Option<(f64, f64)>,
    pub off: Option<(f64, f64)>,
    pub rot: Option<Rot>,
    pub scroll: Option<(f64, f64)>,
}

/// The non-identity parts of a slot's `<TexMod>` element, or None when it does nothing.
///
/// Layout and units: CONFIRMED against the engine:
/// attributes TileU/V, OffsetU/V, RotateU/V/W (fixed angles, DEGREES), TexMod_RotateType (u8), TexMod_WRotateRate (deg per
/// second), TexMod_URotateCenter / TexMod_VRotateCenter, TexMod_UOscillatorType / Rate / Amplitude / Phase (and V).  Meanings of
/// the types are INFERRED from open CryEngine (the runtime evaluation was not located): RotateType 0 none, 1 fixed, 2 constant
/// rate, 3 oscillated; OscillatorType 2 = constant scroll.
pub fn texmod(tm: &Element) -> Option<TexMod> {
    let f = |k: &str, d: f64| float(tm.get(k), d);
    let mut out = TexMod::default();
    let tile = (f("TileU", 1.0), f("TileV", 1.0));
    let off = (f("OffsetU", 0.0), f("OffsetV", 0.0));
    if tile != (1.0, 1.0) {
        out.tile = Some(tile);
    }
    if off != (0.0, 0.0) {
        out.off = Some(off);
    }
    let rtype = f("TexMod_RotateType", 0.0) as i64;
    if (rtype == 1 || rtype == 2) && (f("RotateW", 0.0) != 0.0 || (rtype == 2 && f("TexMod_WRotateRate", 0.0) != 0.0)) {
        out.rot = Some(Rot {
            rtype,
            w: f("RotateW", 0.0),
            rate: if rtype == 2 { f("TexMod_WRotateRate", 0.0) } else { 0.0 },
            c: (f("TexMod_URotateCenter", 0.5), f("TexMod_VRotateCenter", 0.5)),
        });
    }
    let scroll = (
        if f("TexMod_UOscillatorType", 0.0) as i64 == 2 { f("TexMod_UOscillatorRate", 0.0) } else { 0.0 },
        if f("TexMod_VOscillatorType", 0.0) as i64 == 2 { f("TexMod_VOscillatorRate", 0.0) } else { 0.0 },
    );
    if scroll != (0.0, 0.0) {
        out.scroll = Some(scroll);
    }
    if out == TexMod::default() {
        None
    } else {
        Some(out)
    }
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub path: String,
    pub submtl: String,
    pub tint: [f64; 3],
    pub wear_tint: [f64; 3],
    pub gloss_mult: f64,
    pub wear_gloss: f64,
    pub tiling: f64,
    pub palette_tint: i64,
    pub height_bias: f64,
    pub height_scale: f64,
}

#[derive(Debug, Clone)]
pub struct SubMaterial {
    pub name: String,
    pub shader: String,
    pub surface: String,
    pub gen_mask: String,
    /// {TexSlotN: file} in document order
    pub textures: Vec<(String, String)>,
    pub tiles: Vec<(String, (f64, f64))>,
    pub texmods: Vec<(String, TexMod)>,
    pub layers: Vec<Layer>,
    /// PublicParams attributes (strings)
    pub params: Vec<(String, String)>,
    pub diffuse: [f64; 3],
    pub opacity: f64,
    pub emissive: [f64; 3],
    pub specular: [f64; 3],
    pub shininess: f64,
    pub alpha_test: f64,
    pub glow: f64,
    /// `GlowAmount`, None when absent (the engine prefers it to Glow, CONFIRMED)
    pub glow_amount: Option<f64>,
    pub diffuse_tex: Option<String>,
}

impl SubMaterial {
    pub fn texture(&self, slot: &str) -> Option<&str> {
        self.textures.iter().find(|(k, _)| k == slot).map(|(_, v)| v.as_str())
    }
}

/// `float(m.get(key) or default)`: an empty or missing attribute gives the default, a malformed one raises.
fn strict_float(m: &Element, key: &str, default: f64) -> Result<f64> {
    match m.get(key) {
        None | Some("") => Ok(default),
        Some(s) => py_float(s).ok_or_else(|| Error::msg(format!("ValueError: could not convert string to float: '{}'", s))),
    }
}

/// List of sub-material dicts (a lone material counts as one).  EMPIRICAL (files only; the shader semantics are documented where
/// they are used).  Besides the flat-colour fields: gen_mask (StringGenMask), textures {TexSlotN: file}, tiles {TexSlotN: (TileU,
/// TileV)} from the slots' TexMod, layers from `<MatLayers>` and params (PublicParams attributes, strings), glow and glow_amount.
pub fn parse_mtl(data: &[u8]) -> Result<Vec<SubMaterial>> {
    let root = cryxml::parse(data)?;
    let mats: Vec<&Element> = match root.find("SubMaterials") {
        Some(s) => s.children.iter().collect(),
        None => vec![&root],
    };
    let mut out = Vec::with_capacity(mats.len());
    for m in mats {
        let (mut tex, mut tiles, mut texmods): (Vec<(String, String)>, Vec<(String, (f64, f64))>, Vec<(String, TexMod)>) = (vec![], vec![], vec![]);
        fn set<T>(v: &mut Vec<(String, T)>, k: String, val: T) {
            match v.iter_mut().find(|(ek, _)| *ek == k) {
                Some(slot) => slot.1 = val,
                None => v.push((k, val)),
            }
        }
        if let Some(t) = m.find("Textures") {
            for el in &t.children {
                if let Some(file) = el.get("File").filter(|f| !f.is_empty()) {
                    let map = el.get("Map").unwrap_or("None").to_string();
                    set(&mut tex, map.clone(), file.to_string());
                    if let Some(tm) = el.find("TexMod") {
                        set(&mut tiles, map.clone(), (float(tm.get("TileU"), 1.0), float(tm.get("TileV"), 1.0)));
                        if let Some(md) = texmod(tm) {
                            set(&mut texmods, map, md);
                        }
                    }
                }
            }
        }
        let mut layers = vec![];
        if let Some(ml) = m.find("MatLayers") {
            for el in &ml.children {
                layers.push(Layer {
                    name: el.get("Name").unwrap_or("").to_string(),
                    path: el.get("Path").unwrap_or("").to_string(),
                    submtl: el.get("Submtl").unwrap_or("").to_string(),
                    tint: floats(el.get("TintColor"), [1.0; 3]),
                    wear_tint: floats(el.get("WearTint"), [1.0; 3]),
                    gloss_mult: float(el.get("GlossMult"), 1.0),
                    wear_gloss: float(el.get("WearGloss"), 1.0),
                    tiling: float(el.get("UVTiling"), 1.0),
                    palette_tint: float(el.get("PaletteTint"), 0.0) as i64,
                    height_bias: float(el.get("HeightBias"), 0.0),
                    height_scale: float(el.get("HeightScale"), 1.0),
                });
            }
        }
        let diffuse_tex = tex
            .iter()
            .find(|(k, _)| k == "TexSlot1")
            .or_else(|| tex.iter().find(|(k, _)| k == "Diffuse"))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty());
        out.push(SubMaterial {
            name: m.get("Name").unwrap_or("").to_string(),
            shader: m.get("Shader").unwrap_or("").to_string(),
            surface: m.get("SurfaceType").unwrap_or("").to_string(),
            gen_mask: m.get("StringGenMask").unwrap_or("").to_string(),
            textures: tex,
            tiles,
            texmods,
            layers,
            params: m.find("PublicParams").map(|p| p.attrs.clone()).unwrap_or_default(),
            diffuse: floats(m.get("Diffuse"), [1.0; 3]),
            opacity: strict_float(m, "Opacity", 1.0)?,
            emissive: floats(m.get("Emissive"), [0.0; 3]),
            specular: floats(m.get("Specular"), [0.04; 3]),
            shininess: strict_float(m, "Shininess", 0.0)?,
            alpha_test: strict_float(m, "AlphaTest", 0.0)?,
            glow: float(m.get("Glow"), 0.0),
            glow_amount: m.get("GlowAmount").and_then(py_float),
            diffuse_tex,
        });
    }
    Ok(out)
}
