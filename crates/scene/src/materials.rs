//! Materials and textures used by a scene.
//!
//! Evidence (checked on real assets, not against the engine loader):
//!   * a .cgf stores its material as a path string without extension in chunk 0x83353333; the file is `<path>.mtl`;
//!   * mesh subsets pick a sub-material with the LOW 16 bits of Subset.mat_id (the high 16 bits are the node index);
//!   * material colours are "r,g,b" floats taken as linear and shown through a 1/2.2 gamma; TexSlot1 is the diffuse map; its File
//!     ends in .tif but the archive stores .dds;
//!   * DDS: the base file holds the header and the smallest mips, the larger mips live in `<name>.dds.N` (see `textures`).
//!
//! The registry produces the `materials` / `textures` arrays of the viewer's scene JSON as `serde_json::Value`s with the same
//! keys, key order and values as the original Python implementation, which the viewer was written against; floats are rounded
//! like Python's `round(x, n)`.  The docs below mark each statement CONFIRMED (checked against the engine), EMPIRICAL (counted
//! over the archive) or INFERRED.
//!
//! Two registry modes: the default embeds the textures as base64 (`with_textures`, the single-asset page: decoded
//! BC1/2/3 as PNG, BC7 raw), `lazy_textures` lists `{path, chan}` descriptions instead (the zone / item pages fetch them through
//! `texserve`).  The Game2.dcb item-record lookup (`items`) is an optional trait object.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use base64::Engine;
use p4k_formats::mtl::{self, py_float, Layer, SubMaterial, TexMod};
use p4k_formats::surfacetypes::{self, Surface};
use p4k::P4k;
use rayon::prelude::*;
use serde_json::{json, Map, Value};

use crate::mfd;
use crate::textures::{self, texture_base, DdsTexture};

pub const SURFACE_TYPES_FILE: &str = "Data/Libs/MaterialEffects/SurfaceTypes.xml";

/// `{surface name (lower case): {coll, flags}}` of the archive's SurfaceTypes.xml ({} if missing or unreadable).
pub fn surface_table(p4k: &P4k) -> HashMap<String, Surface> {
    p4k.find_path(SURFACE_TYPES_FILE, ".xml")
        .and_then(|e| p4k.read(e).ok())
        .and_then(|d| surfacetypes::parse_surface_types(&d).ok())
        .unwrap_or_default()
}

pub use crate::items::ItemMaterialSource;

// ---------------------------------------------------------------------------------------------------------------------------
// small numeric helpers

/// Rounding like Python's `round(x, nd)`: the float nearest the correctly rounded decimal (ties to even on the exact binary value).
pub fn rnd(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", nd, x).parse().unwrap_or(x)
}

fn cl(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}

fn gamma1(x: f64) -> f64 {
    cl(x, 0.0, 1.0).powf(1.0 / 2.2)
}

/// `[round(x, 4) for x in _gamma(c)]`
fn g4(c: &[f64]) -> Value {
    Value::Array(c.iter().map(|&x| json!(rnd(gamma1(x), 4))).collect())
}

/// `[x ...]` rounded to 4 digits without the gamma
fn r4s(c: &[f64]) -> Value {
    Value::Array(c.iter().map(|&x| json!(rnd(x, 4))).collect())
}

fn param<'a>(params: &'a [(String, String)], k: &str) -> Option<&'a str> {
    params.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str())
}

/// `float(params.get(k, d))` with a default for a missing or malformed value.
pub fn num_param(params: &[(String, String)], k: &str, d: f64) -> f64 {
    param(params, k).and_then(py_float).unwrap_or(d)
}

/// `float(params.get(k) or d)`: an empty or missing value gives `d`; a malformed one is an Err.
fn por(params: &[(String, String)], k: &str, d: f64) -> Result<f64, ()> {
    match param(params, k) {
        None | Some("") => Ok(d),
        Some(v) => py_float(v).ok_or(()),
    }
}

/// `"r,g,b"` as exactly three floats, else None.
fn vec3(params: &[(String, String)], k: &str) -> Option<[f64; 3]> {
    let v: Option<Vec<f64>> = param(params, k)?.split(',').map(py_float).collect();
    let v = v?;
    if v.len() == 3 {
        Some([v[0], v[1], v[2]])
    } else {
        None
    }
}

fn gen_has(gen_mask: &str, tok: &str) -> bool {
    gen_mask.split('%').any(|t| t == tok)
}

/// File-name suffix of a texture path ('ddna' of '.../x_ddna.tif'), lower case; "" for placeholders.
fn suffix(path: &str) -> String {
    if path.is_empty() || path.starts_with('$') {
        return String::new();
    }
    let p = path.replace('\\', "/");
    let name = p.rsplit('/').next().unwrap_or("");
    let stem = name.rsplit_once('.').map_or(name, |x| x.0);
    stem.rsplit('_').next().unwrap_or("").to_lowercase()
}

/// The last '_'-separated part of a texture name, lower-cased and without extension (the slot-name test).
fn name_tail(n: &str) -> String {
    let l = n.to_lowercase();
    let last = l.rsplit('_').next().unwrap_or("");
    last.split('.').next().unwrap_or("").to_string()
}

fn is_ddna(n: &str) -> bool {
    matches!(name_tail(n).as_str(), "ddna" | "ddn")
}


// ---------------------------------------------------------------------------------------------------------------------------
// shader rules

pub const GLASS_ALPHA: f64 = 0.3; // guessed alpha of GlassPBR surfaces (see resolve)
pub const ALWAYS_BLENDED: [&str; 2] = ["GlassPBR", "VFX_Transparent"];

/// Shader families the viewer has no program for, mapped onto the closest existing one (`fx` of the material description).  The
/// engine's own programs (scan lines, holo interference, beam noise, cloth sheen) are NOT evaluated; the mapping only gets the coarse
/// look right.  INFERRED from the shader names and their .mtl contents (all ship / vehicle .mtl files):
///   "screen"  Monitor: a self-lit display; TexSlot1 is the picture tinted by Diffuse.  Drawn as the lit diffuse plus the same colour
///             again as self-illumination (like the MFD canvases).
///   "add"     holograms, UI meshes and light beams: no normal / specular slots, a Glow value, Opacity 0.3 .. 1, gradient / noise
///             textures in TexSlot1: drawn additively, self-lit, no depth write, in the translucent pass.
///   "illum"   Cloth / Vegetation: TexSlot1 'diff', TexSlot2 'ddna' (the Illum slots, EMPIRICAL on the files): drawn with the Illum maps.
pub const SHADER_FX: [(&str, &str); 10] = [
    ("Monitor", "screen"),
    ("UIMesh", "add"),
    ("Hologram", "add"),
    ("HologramCIG", "add"),
    ("GeometryBeam", "add"),
    ("TemplBeamProc", "add"),
    ("Ping", "add"),
    ("Lightning", "add"),
    ("Cloth", "illum"),
    ("Vegetation", "illum"),
];

pub fn shader_fx(shader: &str) -> Option<&'static str> {
    SHADER_FX.iter().find(|(k, _)| *k == shader).map(|(_, v)| *v)
}

/// Shaders the zone viewer has a rule for; any other shader is drawn by the plain program as colour * TexSlot1.
pub fn is_supported_shader(shader: &str) -> bool {
    ["Illum", "MeshDecal", "LayerBlend_V2", "HardSurface", "GlassPBR", "NoDraw", "DisplayScreen", "UIPlane"].contains(&shader) || shader_fx(shader).is_some()
}

/// A MeshDecal whose diffuse is the engine placeholder '$RenderToTexture' (EMPIRICAL: the 'RTT_Text_To_Decal' sub-materials of ship
/// hulls, %USE_RTT_UV_FOR_DIFFUSE: the ship's name painted at run time by a RenderToTexture entity).  The viewer has no such texture
/// and drew the decal quad in its flat Diffuse colour; these ranges are left out of the zone meshes instead.
pub fn is_rtt_text(s: &SubMaterial) -> bool {
    s.shader.to_lowercase() == "meshdecal" && s.texture("TexSlot1").unwrap_or("").to_lowercase().starts_with("$rendertotexture")
}

/// MatEmissiveColor.a of sub-material `s`: (20 g)^2 with g = GlowAmount * 0.1 when the material has GlowAmount, else the raw `Glow`
/// attribute (CONFIRMED against the engine's material loader: median Glow 0.09 gives 3.24, GlowAmount 1 gives 4, Glow 1 gives 400).
/// The LayerBlend_V2 per-material data writes (20 * GlowAmount)^2; whether GlowAmount there is already the converted g is not
/// traced, the conversion above is used for every shader (only 45 ship LayerBlend sub-materials have a glow at all, all with a plain `Glow` and no GlowAmount, so the
/// ambiguity does not matter for them).
pub fn glow_intensity(s: &SubMaterial) -> f64 {
    let g = match s.glow_amount {
        Some(ga) => ga * 0.1,
        None => s.glow,
    };
    if g > 0.0 {
        (20.0 * g).powi(2)
    } else {
        0.0
    }
}

/// How a sub-material emits light (`emission`): kind 'd' = the lit / blended diffuse is the emission colour, 'm' = HardSurface
/// %MACRO_EMISSIVE_MAP (the colour is EmissiveMap x the .mtl Emissive rgb), `a` = MatEmissiveColor.a = glow_intensity, `cut` =
/// AlphaGlowCutoff (the 'd' mask of HardSurface / Illum), `far` = [FarGlowStartDistance, FarGlowEndDistance, FarGlowMultiplier].
#[derive(Debug, Clone)]
pub struct Emission {
    pub kind: char,
    pub a: f64,
    pub cut: f64,
    pub far: Option<[f64; 3]>,
    pub col: [f64; 3],
}

impl Emission {
    /// The viewer's `emit` object; `col` (kind 'm') as given (the registry passes the gamma-rounded colour).
    fn to_value(&self, col: Option<Value>) -> Value {
        let mut m = Map::new();
        m.insert("k".into(), json!(self.kind.to_string()));
        m.insert("a".into(), json!(self.a));
        if self.kind == 'm' {
            m.insert("col".into(), col.unwrap_or_else(|| json!(self.col)));
        }
        m.insert("cut".into(), json!(self.cut));
        if let Some(f) = self.far {
            m.insert("far".into(), json!(f));
        }
        Value::Object(m)
    }
}

/// How sub-material `s` emits light, or None when it does not (replaces the old `emission_driven` placeholder test).
///
/// CONFIRMED against the LayerBlend, HardSurface and Illum pixel shaders in Engine/ShaderCache_Vulkan.pak and the engine's
/// per-material data:
///   * LayerBlend_V2: emission = E * ((20 * GlowAmount)^2 + [material flag 0x8000] instanceGlow * GlowFactor) * (1 - dirt); E =
///     EmissiveMap.rgb when the EmissiveMode param is non-zero, else the blended layer diffuse after the wear mix.  The .mtl Emissive
///     rgb is NEVER read: that is why the Emissive="1,1,1" placeholder on every power plant / cooler / shield / quantum-drive
///     sub-material (EMPIRICAL census, 2,736 ship sub-materials, all Glow 0) painted components white in the viewer and must not
///     emit.  Glow 0 means no static emission (only the per-instance glow colour: thrusters, lights).  EmissiveMode is 0 on all
///     6,378 ship LayerBlend sub-materials (EMPIRICAL census), so the EmissiveMap branch is not drawn; only the 'd' form exists.
///   * HardSurface: default path = litDiffuse * clamp((diffuse.a - AlphaGlowCutoff) / (1 - AlphaGlowCutoff)) * [VERTCOLORS g] *
///     (instanceGlow * GlowFactor + MatEmissiveColor.a); with MACRO_EMISSIVE_MAP the colour is EmissiveArrayMap * EmissiveMap *
///     MatEmissiveColor.rgb.  Illum (I_8) has the default formula, so the Emissive rgb matters only for HardSurface
///     MACRO_EMISSIVE_MAP.
///   * FarGlow: far = clamp((viewDist - FarGlowStart) / (FarGlowEnd - FarGlowStart)); intensity = mix(a, 400 * (sqrt(a) * 0.05 *
///     FarGlowMultiplier)^2, far) = a * FarGlowMultiplier^2 at far (evaluated in the zone viewer).
/// The per-instance glow colour does not exist in the viewer.  VIEWER CHOICE: a macro-emissive HardSurface is drawn with instance
/// glow 1 (lit), so its intensity is a + 1; every other form assumes instance glow 0.  Other shaders are not touched here (None).
pub fn emission(s: &SubMaterial) -> Option<Emission> {
    let sh = s.shader.as_str();
    if !["LayerBlend_V2", "HardSurface", "Illum"].contains(&sh) {
        return None;
    }
    let a = glow_intensity(s);
    let macro_map = sh == "HardSurface" && gen_has(&s.gen_mask, "MACRO_EMISSIVE_MAP") && s.texture("TexSlot14").is_some();
    let kind = if macro_map {
        'm'
    } else if a > 0.0 {
        'd'
    } else {
        return None;
    };
    let cut = if sh != "LayerBlend_V2" { cl(num_param(&s.params, "AlphaGlowCutoff", 0.0), 0.0, 0.999) } else { 0.0 };
    let far = param(&s.params, "FarGlowMultiplier").map(|_| {
        [num_param(&s.params, "FarGlowStartDistance", 0.0), num_param(&s.params, "FarGlowEndDistance", 0.0), num_param(&s.params, "FarGlowMultiplier", 1.0)]
    });
    Some(Emission { kind, a, cut, far, col: s.emissive })
}

pub fn reinhard(e: f64) -> f64 {
    if e > 0.0 {
        e / (1.0 + e)
    } else {
        0.0
    }
}

/// Constant linear-ish emission colour for the viewers without the per-pixel emission (single-asset, weapon and gear pages): the
/// material colour times the tone-mapped intensity ('d'), the Emissive colour ('m'); zero without emission.  The zone viewer ignores it.
pub fn emission_fallback_rgb(s: &SubMaterial, em: Option<&Emission>) -> [f64; 3] {
    let Some(em) = em else { return [0.0; 3] };
    if em.kind == 'm' {
        return em.col;
    }
    let k = reinhard(em.a);
    [s.diffuse[0] * k, s.diffuse[1] * k, s.diffuse[2] * k]
}

pub const HAIR_ALPHA_TEST: f64 = 0.25; // HairPS: discard when OpacityMap.r < 0.25 (CONFIRMED)
const HAIR_SIGMA_BROWN: [f64; 3] = [0.5028, 0.8364, 1.644]; // eumelanin absorption, per colour channel (HairPS constants, CONFIRMED)
const HAIR_SIGMA_RED: [f64; 3] = [0.339056, 0.946381, 3.482497]; // the red (pheomelanin) mix

/// Hair-colour inputs of a HairPBR hair-card sub-material (`hairpbr_params`).
#[derive(Debug, Clone)]
pub struct Hair {
    pub mel: [f64; 3],
    pub tint: [f64; 3],
    pub mul: [f64; 3],
    pub dye: [f64; 4],
    pub dcol: [f64; 3],
    pub cards: bool,
}

/// Hair-colour inputs of a HairPBR hair-card sub-material, or None when it is not one (then the plain path applies).
///
/// Why: HairPBR %HAIR_CARDS materials bind TexSlot1 to an OPACITY map ('hair_texture_01_opac') and TexSlot4 to a card-id map, and
/// their Diffuse is a neutral grey (0.50), so the generic 'colour x TexSlot1' drew every helmet / head hair grey and every card as a
/// solid quad.  EMPIRICAL: 3,598 HairPBR sub-materials, TexSlot1 file suffix 'opac' in 1,735, 'scalp' 1,329, 'shaved' 72, 'shadow'
/// 26, 'diff' only in 219 (those keep the plain path).  The colour is computed from the parameters by `hairpbr_albedo` (HairPS,
/// CONFIRMED).  Defaults for a missing parameter
/// (BaseMelanin 0.5, Redness 0, variation 0, tint white, no dye) are INFERRED; a material with neither BaseTintColor nor
/// BaseMelanin is left to the plain path.
pub fn hairpbr_params(s: &SubMaterial) -> Option<Hair> {
    if s.shader != "HairPBR" {
        return None;
    }
    if suffix(s.texture("TexSlot1").unwrap_or("")) == "diff" {
        return None;
    }
    let pr = &s.params;
    if param(pr, "BaseTintColor").is_none() && param(pr, "BaseMelanin").is_none() {
        return None;
    }
    let v3 = |k: &str, d: [f64; 3]| vec3(pr, k).unwrap_or(d);
    Some(Hair {
        mel: [num_param(pr, "BaseMelanin", 0.5), num_param(pr, "BaseMelaninVariation", 0.0), num_param(pr, "BaseMelaninRedness", 0.0)],
        tint: v3("BaseTintColor", [1.0; 3]),
        mul: s.diffuse,
        dye: [num_param(pr, "DyeAmount", 0.0), num_param(pr, "DyePigmentVariation", 0.0), num_param(pr, "DyeShift", 0.0), num_param(pr, "DyeFadeout", 0.0)],
        dcol: v3("DyeColor", [1.0; 3]),
        cards: gen_has(&s.gen_mask, "CARD_ID_MAP"),
    })
}

/// Linear albedo of HairPBR parameters `h` for a card id (HairIDMap .r; 0.5 without %CARD_ID_MAP) and the strand coordinate
/// (COLOR0.x, 0 root .. 1 tip).  CONFIRMED formulas (HairPS): m = clamp(BaseMelanin + (id-0.5) *
/// BaseMelaninVariation * 0.5); k = -ln(max(1-m^2, 1e-4)); sigma = brown*k*(1-Redness) + red*k*Redness; rgb = exp(-5.8884
/// sqrt(sigma)) * BaseTintColor.  Dye (only when DyeAmount > 0): dye = clamp(sqrt(DyeColor) + (0.5-id)*0.75*DyePigmentVariation,
/// 1e-4, 0.99)^2, amount = DyeAmount * mask (the DyeColorMap, TEXCOORD1, is not read here: mask 1, INFERRED default of an unbound
/// map), w = mix(sqrt(clamp((amount - id)*2)), 1, amount^2), dyed = mix(melanin, dye, w), t = smoothstep(0, max(DyeFadeout, 1e-3),
/// clamp(max(strand, 1e-3) - DyeShift)), rgb = mix(melanin, dyed, t).  Albedo = clamp(rgb * MatDifColor, 1e-4, 0.99).
/// The item viewer evaluates the same formulas in GLSL.
pub fn hairpbr_albedo(h: &Hair, card_id: f64, strand: f64) -> [f64; 3] {
    let [mb, var, red] = h.mel;
    let m = cl(mb + (card_id - 0.5) * var * 0.5, 0.0, 1.0);
    let k = -(1.0 - m * m).max(1e-4).ln();
    let mut mel = [0.0; 3];
    for i in 0..3 {
        mel[i] = (-5.8884 * (HAIR_SIGMA_BROWN[i] * k * (1.0 - red) + HAIR_SIGMA_RED[i] * k * red).sqrt()).exp() * h.tint[i];
    }
    let mut rgb = mel;
    let [amount0, pigvar, shift, fade] = h.dye;
    if amount0 > 0.0 {
        let mut dye = [0.0; 3];
        for i in 0..3 {
            dye[i] = cl(h.dcol[i].sqrt() + (0.5 - card_id) * 0.75 * pigvar, 1e-4, 0.99).powi(2);
        }
        let w = cl((amount0 - card_id) * 2.0, 0.0, 1.0).sqrt() * (1.0 - amount0.powi(2)) + amount0.powi(2);
        let x = cl(cl(strand.max(1e-3) - shift, 0.0, 1.0) / fade.max(1e-3), 0.0, 1.0);
        let tt = x * x * (3.0 - 2.0 * x);
        for i in 0..3 {
            rgb[i] = mel[i] + (dye[i] - mel[i]) * w * tt;
        }
    }
    [cl(rgb[0] * h.mul[0], 1e-4, 0.99), cl(rgb[1] * h.mul[1], 1e-4, 0.99), cl(rgb[2] * h.mul[2], 1e-4, 0.99)]
}

/// Whether the HardSurface Secondary layer whose layer file is `f` is drawn: whenever the layer file resolves.  Its blend map
/// (TexSlot8: R = height, G = softness) may be missing (~27% of unique HardSurface layer .mtl have no TexSlot8): the engine then
/// binds a default texture, INFERRED black (the default-texture name is not traced), so the viewer samples (0, 0) there
/// instead of leaving the layer out.
pub fn secondary_drawn(f: Option<&SubMaterial>) -> bool {
    f.is_some()
}

/// Names of the features of sub-material `s` that the zone viewer does NOT draw (for a survey of the gaps).
/// `layer_file(path, submtl)` returns a layer's sub-material or None.  The list follows the viewer: when a feature gets drawn its
/// entry goes away here.  EMPIRICAL slot meanings, see the methods of `MatRegistry`.  A HardSurface layer map whose permutation
/// token is missing (%USE_SPECULAR_MAPS / %USE_DETAIL_MAPS) is not counted: that permutation does not read it (INFERRED).
pub fn undrawn_features(s: &SubMaterial, layer_file: &mut dyn FnMut(&str, &str) -> Option<SubMaterial>) -> Vec<String> {
    let mut out = vec![];
    let sh = s.shader.as_str();
    if sh == "HardSurface" || sh == "LayerBlend_V2" {
        if gen_has(&s.gen_mask, "IRIDESCENCE") {
            // HardSurface iridescence is drawn (a missing TexSlot10 falls back to the system gradient); LayerBlend_V2 is not done
            if sh == "LayerBlend_V2" {
                out.push("lb-iridescence".to_string());
            }
        }
        // LayerBlend_V2 layers: the layer file's specular (TexSlot6) and detail (TexSlot7) maps are NOT gaps.  CONFIRMED (all 64
        // LayerBlend pixel-shader permutations): the only texture arrays are diffuseTexArray / normalTexArray /
        // glossTexArray, there is no detail or specular sampler in any of them; the engine does not draw a LayerBlend layer
        // detail / specular map; neither does the viewer.
        for l in &s.layers {
            if l.name == "Secondary" && !l.path.is_empty() && gen_has(&s.gen_mask, "SECONDARY_LAYER") {
                let f = layer_file(&l.path, &l.submtl);
                if !secondary_drawn(f.as_ref()) {
                    out.push("hs-secondary".to_string());
                }
            }
        }
    }
    if sh == "GlassPBR" {
        // THIN_FILM is drawn (system texture thinfilm.dds, sc-45); REFRACTION / CRACKS: not drawn
        for t in ["REFRACTION", "CRACKS"] {
            if gen_has(&s.gen_mask, t) {
                out.push(format!("glass-{}", t.to_lowercase()));
            }
        }
    }
    if sh == "DisplayScreen" && !s.textures.iter().any(|(_, v)| v.to_lowercase() == "$rendertotexture") {
        out.push("static-screen".to_string());
    }
    out
}

pub const CUTOUT_SKIP_SHADERS: [&str; 10] = ["MeshDecal", "Decal", "WaterDecal", "GlassPBR", "VFX_Transparent", "NoDraw", "UIPlane", "UIMesh", "Hologram", "HologramCIG"];

/// Whether a sub-material is drawn in the blended pass (the zone viewer reads the `trans` flag set from this; the single-asset page uses opacity).
/// Shaders named for transparency (ALWAYS_BLENDED) are blended whatever the .mtl says: EMPIRICAL over the 1,426 ship .mtl files,
/// GlassPBR is Opacity 1.0 in 2,900 of 3,088 sub-materials (transparency comes from the shader) and a few carry a stray
/// AlphaTest (Idris med bay panes: 1.0), which the old rule read as a cut-out request and drew the pane opaque; 16 of 29
/// VFX_Transparent materials had the same stray AlphaTest=1.  Other shaders blend when Opacity < 0.99 unless an alpha test cut-out
/// is requested (INFERRED: AlphaTest > 0 selects alpha-to-coverage / cut-out in the engine, not blending).
pub fn is_translucent(shader: &str, opacity: f64, alpha_test: f64) -> bool {
    if ALWAYS_BLENDED.contains(&shader) {
        return true;
    }
    opacity < 0.99 && alpha_test <= 0.0
}

pub const VCOL_NONE: i64 = 0;
pub const VCOL_RGB: i64 = 1;
pub const VCOL_DAMAGE: i64 = 2;
pub const VCOL_ALPHA: i64 = 3;
pub const VCOL_MD: i64 = 4;
pub const VCOL_MDATA: i64 = 5;

/// How a material uses the vertex colour (read as r,g,b,a after `ivo::bgra_to_rgba`).
///
/// Illum pixel shaders (CONFIRMED; the engine's vertex declaration was not checked): Illum + %VERTCOLORS alone -> VCOL_RGB:
/// diffuse and glow colour times COLOR0.rgb, COLOR0.a times the glow / decal alpha.  Illum + %VERTCOLORS + %USE_DAMAGE_MAP ->
/// VCOL_DAMAGE: diffuse *= .b (scalar), glow *= .g (scalar), alpha *= .a, .r is a damage threshold (no damage here, ignored).
/// MeshDecal (MeshDecalPS, CONFIRMED): %VERTCOLORS -> VCOL_MD: COLOR0.rgb multiplies the diffuse and
/// COLOR0.a the opacity (white on 1,539 ships' decals, EMPIRICAL); %VERTDATA -> VCOL_MDATA: COLOR0 is data
/// (a = wear / dirt mask, g = glow mask, b = tint palette index), it never tints or fades the decal.  Other shaders with
/// %VERTCOLORS -> VCOL_ALPHA: only the alpha is used here (HardSurface / LayerBlend are handled by the layered program, which reads
/// .a and .b).
pub fn vcol_mode(shader: &str, gen_mask: &str) -> i64 {
    if shader.to_lowercase() == "meshdecal" {
        return if gen_has(gen_mask, "VERTCOLORS") {
            VCOL_MD
        } else if gen_has(gen_mask, "VERTDATA") {
            VCOL_MDATA
        } else {
            VCOL_NONE
        };
    }
    if !gen_has(gen_mask, "VERTCOLORS") {
        return VCOL_NONE;
    }
    if shader.to_lowercase() != "illum" {
        return VCOL_ALPHA;
    }
    if gen_has(gen_mask, "USE_DAMAGE_MAP") {
        VCOL_DAMAGE
    } else {
        VCOL_RGB
    }
}

pub const DECAL_KEY_TOKENS: [&str; 13] = [
    "DECAL", "DECALS", "VERTCOLORS", "VERTDATA", "USE_DAMAGE_MAP", "PARALLAX_OCCLUSION_MAPPING", "BLENDLAYER", "DISABLE_ALPHA_BLEND", "STENCIL_MAP", "STENCIL_AS_STICKER",
    "USE_RTT_UV_FOR_DIFFUSE", "DECAL_OPACITY_MAP", "NORMAL_MAP",
];

/// 'Shader | tokens' key of a decal-like material (the zone viewer's census counts ranges by it).
pub fn decal_variant(shader: &str, gen_mask: &str) -> String {
    let t: Vec<&str> = DECAL_KEY_TOKENS.iter().copied().filter(|k| gen_has(gen_mask, k)).collect();
    format!("{} | {}", shader, if t.is_empty() { "-".to_string() } else { t.join(" ") })
}

fn texmod_value(t: &TexMod) -> Value {
    let mut m = Map::new();
    if let Some((u, v)) = t.tile {
        m.insert("tile".into(), json!([u, v]));
    }
    if let Some((u, v)) = t.off {
        m.insert("off".into(), json!([u, v]));
    }
    if let Some(r) = &t.rot {
        m.insert("rot".into(), json!({"type": r.rtype, "w": r.w, "rate": r.rate, "c": [r.c.0, r.c.1]}));
    }
    if let Some((u, v)) = t.scroll {
        m.insert("scroll".into(), json!([u, v]));
    }
    Value::Object(m)
}

// ---------------------------------------------------------------------------------------------------------------------------
// the registry

/// Collects the materials/textures a scene uses; `part.mat` indexes `materials` (-1 = none).
pub struct MatRegistry<'a> {
    pub p4k: &'a P4k,
    pub max_tex: usize,
    pub with_textures: bool,
    /// textures[i] is `{path, chan}` for a diffuse map that exists in the archive; the data is not read (the zone viewer fetches it
    /// separately, see `texserve`)
    pub lazy_textures: bool,
    /// the Game2.dcb item records' material pairing (None for the command line tools)
    pub items: Option<&'a dyn ItemMaterialSource>,
    pub materials: Vec<Value>,
    pub textures: Vec<Value>,
    mtl: HashMap<String, Option<Arc<Vec<SubMaterial>>>>,
    mat_idx: HashMap<(String, i64), i64>,
    tex_idx: HashMap<(String, String), i64>,
    cov_idx: HashMap<(String, String), Option<f64>>,
    /// decoded embedded textures keyed like `tex_idx` (filled in parallel before a material is described)
    decoded: HashMap<(String, String), Option<Value>>,
    pub notes: BTreeSet<String>,
    surfaces: Option<HashMap<String, Surface>>,
}

type Sub = SubMaterial;

impl<'a> MatRegistry<'a> {
    /// The defaults: `max_tex` 256, textures embedded (`with_textures`), not lazy, no item records.
    pub fn new(p4k: &'a P4k) -> Self {
        Self::with_options(p4k, 256, true, false)
    }

    pub fn with_options(p4k: &'a P4k, max_tex: usize, with_textures: bool, lazy_textures: bool) -> Self {
        MatRegistry {
            p4k,
            max_tex,
            with_textures,
            lazy_textures,
            items: None,
            materials: vec![],
            textures: vec![],
            mtl: HashMap::new(),
            mat_idx: HashMap::new(),
            tex_idx: HashMap::new(),
            cov_idx: HashMap::new(),
            decoded: HashMap::new(),
            notes: BTreeSet::new(),
            surfaces: None,
        }
    }

    /// The id-bearing state as a lossless JSON value: `{m: materials, t: textures, mi: [[file, sub, id]], ti: [[path, chan, id]]}`.
    /// Only entries that were given an id are listed (a `-1` is a negative cache that recomputes to the same answer), sorted by id so
    /// the bytes are deterministic.  The disk cache of the zone feed stores this (zonecache.rs): material and texture ids are handed
    /// out in request order, so a stored geometry blob is only meaningful with the registry it was built against.
    pub fn state_json(&self) -> Value {
        let mut mi: Vec<(&(String, i64), &i64)> = self.mat_idx.iter().filter(|(_, i)| **i >= 0).collect();
        mi.sort_by_key(|(_, i)| **i);
        let mut ti: Vec<(&(String, String), &i64)> = self.tex_idx.iter().filter(|(_, i)| **i >= 0).collect();
        ti.sort_by_key(|(_, i)| **i);
        json!({
            "m": self.materials, "t": self.textures,
            "mi": mi.iter().map(|((f, s), i)| json!([f, s, i])).collect::<Vec<_>>(),
            "ti": ti.iter().map(|((p, c), i)| json!([p, c, i])).collect::<Vec<_>>(),
        })
    }

    /// Takes over a `state_json` snapshot that extends this registry: the materials and textures held now must be a prefix of the
    /// snapshot's (equal entries), else nothing changes and the error says why.  Afterwards every id the snapshot handed out means the
    /// same here, and `resolve` / `texture` reuse them.
    pub fn adopt_state(&mut self, st: &Value) -> Result<(), String> {
        let list = |k: &str| st.get(k).and_then(Value::as_array).ok_or_else(|| format!("snapshot has no '{}'", k));
        let (m, t, mi, ti) = (list("m")?, list("t")?, list("mi")?, list("ti")?);
        if m.len() < self.materials.len() || m[..self.materials.len()] != self.materials[..] {
            return Err("materials are not an extension of the live registry".into());
        }
        if t.len() < self.textures.len() || t[..self.textures.len()] != self.textures[..] {
            return Err("textures are not an extension of the live registry".into());
        }
        let mut mat_idx = vec![];
        for e in mi {
            match (e[0].as_str(), e[1].as_i64(), e[2].as_i64()) {
                (Some(f), Some(s), Some(i)) if i >= 0 && (i as usize) < m.len() => mat_idx.push(((f.to_string(), s), i)),
                _ => return Err("bad material index entry".into()),
            }
        }
        let mut tex_idx = vec![];
        for e in ti {
            match (e[0].as_str(), e[1].as_str(), e[2].as_i64()) {
                (Some(p), Some(c), Some(i)) if i >= 0 && (i as usize) < t.len() => tex_idx.push(((p.to_string(), c.to_string()), i)),
                _ => return Err("bad texture index entry".into()),
            }
        }
        // a live id must not be reassigned
        if mat_idx.iter().any(|(k, i)| self.mat_idx.get(k).is_some_and(|j| *j >= 0 && j != i))
            || tex_idx.iter().any(|(k, i)| self.tex_idx.get(k).is_some_and(|j| *j >= 0 && j != i))
        {
            return Err("an index entry disagrees with the live registry".into());
        }
        self.materials = m.clone();
        self.textures = t.clone();
        self.mat_idx.extend(mat_idx);
        self.tex_idx.extend(tex_idx);
        Ok(())
    }

    /// {global material index: (material file, sub-material)} of everything `resolve` has described so far.
    pub fn mat_keys(&self) -> HashMap<i64, (String, i64)> {
        self.mat_idx.iter().filter(|(_, gi)| **gi >= 0).map(|(k, gi)| (*gi, k.clone())).collect()
    }

    pub fn is_nodraw(&self, gmat: i64) -> bool {
        gmat >= 0 && self.materials.get(gmat as usize).map_or(false, |m| m["nodraw"] == Value::Bool(true))
    }

    pub fn load_mtl(&mut self, mtl_path: &str) -> Option<Arc<Vec<SubMaterial>>> {
        if !self.mtl.contains_key(mtl_path) {
            let mut subs = None;
            match self.p4k.find_path(&format!("{}.mtl", mtl_path), ".mtl") {
                None => {
                    self.notes.insert(format!("material file not in the archive: {}", mtl_path));
                }
                Some(ent) => {
                    // a broken material file must not stop the model from showing
                    match self.p4k.read(ent).map_err(|e| e.to_string()).and_then(|d| mtl::parse_mtl(&d).map_err(|e| e.to_string())) {
                        Ok(s) => subs = Some(Arc::new(s)),
                        Err(ex) => {
                            self.notes.insert(format!("material file unreadable ({}): {}", mtl_path.rsplit('/').next().unwrap(), ex));
                        }
                    }
                }
            }
            self.mtl.insert(mtl_path.to_string(), subs);
        }
        self.mtl[mtl_path].clone()
    }

    /// Embedded form of a texture: `{fmt, w, h, srgb, d}` (None when it cannot be loaded).
    fn embed(p4k: &P4k, path: &str, chan: &str, max_tex: usize) -> Option<Value> {
        let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(d);
        if chan == "ng" || chan == "or" {
            let got = if chan == "ng" { textures::ng_png(p4k, &texture_base(path), max_tex) } else { textures::opacity_png(p4k, &texture_base(path), max_tex) }?;
            return Some(json!({"fmt": "png", "w": got.0, "h": got.1, "srgb": false, "d": b64(&got.2)}));
        }
        let t = textures::load_texture(p4k, path, max_tex)?;
        Some(json!({"fmt": t.fmt, "w": t.w, "h": t.h, "srgb": t.srgb, "d": b64(&t.data)}))
    }

    /// Index of a texture.  chan "a": the alpha / gloss companion file of a ddna map; chan "ng": the ddna normal map and its gloss
    /// packed by the server into one image (R, G = normal x, y; B = gloss; z is rebuilt in the shader); "or": HairPBR opacity
    /// (R -> alpha); "ao": macro AO map as a hue/AO/luminance image; "hw:<wear path>": hue/AO/luminance map packed with the wear
    /// map in its alpha (either file may be absent).
    pub fn texture(&mut self, path: &str, chan: &str) -> i64 {
        if let Some(wear) = chan.strip_prefix("hw:") {
            let p4k = self.p4k;
            let ok = |pth: &str| !pth.is_empty() && p4k.find_path(&format!("{}.dds", texture_base(pth)), ".dds").is_some();
            // the entry is appended again on every call (the hw key is never looked up), kept as is for identical output
            if (ok(path) || ok(wear)) && self.lazy_textures {
                self.textures.push(json!({"path": path, "chan": "hw", "wear": wear}));
                return self.textures.len() as i64 - 1;
            }
            return -1;
        }
        let key = (path.to_string(), chan.to_string());
        if let Some(&i) = self.tex_idx.get(&key) {
            return i;
        }
        let mut idx = -1;
        if self.lazy_textures {
            let base = texture_base(path);
            let alpha = chan == "a";
            if self.p4k.find_path(&format!("{}{}", base, if alpha { ".dds.a" } else { ".dds" }), if alpha { ".a" } else { ".dds" }).is_some() {
                self.textures.push(json!({"path": path, "chan": chan}));
                idx = self.textures.len() as i64 - 1;
            }
        } else if self.with_textures {
            let (p4k, max_tex) = (self.p4k, self.max_tex);
            let got = self.decoded.entry(key.clone()).or_insert_with(|| Self::embed(p4k, path, chan, max_tex)).clone();
            if let Some(t) = got {
                self.textures.push(t);
                idx = self.textures.len() as i64 - 1;
            }
        }
        self.tex_idx.insert(key, idx);
        idx
    }

    /// Decode, in parallel (rayon), the embedded textures sub-material `s` is about to ask for: the order of the registry's texture
    /// indices is decided afterwards by `texture`, so only the (expensive) decoding is concurrent.  Only the non-lazy mode decodes.
    fn prefetch(&mut self, s: &Sub, hair: bool) {
        if self.lazy_textures || !self.with_textures {
            return;
        }
        let mut want: Vec<(String, String)> = vec![];
        if let Some(d) = &s.diffuse_tex {
            if !hair {
                want.push((d.clone(), String::new()));
            }
        }
        if hair {
            if let Some(op) = s.texture("TexSlot1") {
                want.push((op.to_string(), "or".to_string()));
            }
        }
        if s.shader == "GlassPBR" {
            if let Some(n) = s.texture("TexSlot2").filter(|n| gen_has(&s.gen_mask, "NORMAL_MAP") && is_ddna(n)) {
                want.push((n.to_string(), "ng".to_string()));
            }
        }
        if s.shader == "Illum" || s.shader == "MeshDecal" {
            if let Some(n) = s.texture(if s.shader == "MeshDecal" { "TexSlot3" } else { "TexSlot2" }).filter(|n| is_ddna(n)) {
                want.push((n.to_string(), "ng".to_string()));
            }
        }
        want.retain(|k| !self.decoded.contains_key(k));
        want.sort();
        want.dedup();
        if want.len() < 2 {
            return; // nothing to run concurrently
        }
        let (p4k, max_tex) = (self.p4k, self.max_tex);
        let got: Vec<((String, String), Option<Value>)> = want.into_par_iter().map(|k| (k.clone(), Self::embed(p4k, &k.0, &k.1, max_tex))).collect();
        self.decoded.extend(got);
    }

    /// Share of the diffuse map's texels with alpha >= `cut` (see `textures::cutout_coverage`), cached.
    pub fn cutout_coverage(&mut self, path: &str, cut: f64) -> Option<f64> {
        let key = (path.to_string(), format!("{}", rnd(cut, 4)));
        if let Some(c) = self.cov_idx.get(&key) {
            return *c;
        }
        let cov = textures::cutout_coverage(self.p4k, path, cut);
        self.cov_idx.insert(key, cov);
        cov
    }

    /// SurfaceType name of (material file, sub-material), '' when unknown.
    pub fn surface(&mut self, mtl_path: &str, sub_idx: i64) -> String {
        if mtl_path.is_empty() {
            return String::new();
        }
        match self.load_mtl(mtl_path) {
            Some(subs) if sub_idx >= 0 && (sub_idx as usize) < subs.len() => subs[sub_idx as usize].surface.clone(),
            _ => String::new(),
        }
    }

    /// False when the faces of this sub-material do not stop the player, by their surface type (formats/surfacetypes).
    pub fn player_collides(&mut self, mtl_path: &str, sub_idx: i64) -> bool {
        let name = self.surface(mtl_path, sub_idx);
        if name.is_empty() {
            return true;
        }
        let p4k = self.p4k;
        let table = self.surfaces.get_or_insert_with(|| surface_table(p4k));
        surfacetypes::player_collides(table.get(&name.to_lowercase()))
    }

    /// Global material index for (material file, sub-material), or -1 if unknown.
    pub fn resolve(&mut self, mtl_path: &str, sub_idx: i64) -> i64 {
        let key = (mtl_path.to_string(), sub_idx);
        if let Some(&i) = self.mat_idx.get(&key) {
            return i;
        }
        let mut gi = -1;
        if let Some(subs) = self.load_mtl(mtl_path) {
            if sub_idx >= 0 && (sub_idx as usize) < subs.len() {
                let mat = self.describe(&subs[sub_idx as usize]);
                self.materials.push(mat);
                gi = self.materials.len() as i64 - 1;
            }
        }
        self.mat_idx.insert(key, gi);
        gi
    }

    /// The material description of one sub-material.
    fn describe(&mut self, s: &Sub) -> Value {
        let hair = hairpbr_params(s);
        self.prefetch(s, hair.is_some());
        let tex = match &s.diffuse_tex {
            Some(d) if hair.is_none() => self.texture(d, ""),
            _ => -1,
        };
        let sh = s.shader.as_str();
        let mut mat = Map::new();
        mat.insert("name".into(), json!(s.name));
        mat.insert("shader".into(), json!(s.shader));
        mat.insert("nodraw".into(), json!(sh.to_lowercase() == "nodraw"));
        mat.insert("color".into(), g4(&s.diffuse));
        mat.insert("emissive".into(), json!([0.0, 0.0, 0.0]));
        mat.insert("opacity".into(), json!(s.opacity));
        mat.insert("alpha_test".into(), json!(s.alpha_test));
        mat.insert("tex".into(), json!(tex));
        let em = if sh != "MeshDecal" { emission(s) } else { None };
        if let Some(e) = &em {
            mat.insert("emissive".into(), g4(&emission_fallback_rgb(s, Some(e))));
            // the zone viewer evaluates it per pixel
            mat.insert("emit".into(), e.to_value(if e.kind == 'm' { Some(g4(&e.col)) } else { None }));
        } else if !["LayerBlend_V2", "HardSurface", "Illum"].contains(&sh) {
            mat.insert("emissive".into(), g4(&s.emissive)); // Hologram, Monitor, ...: the Emissive colour as before
        }
        if let Some(h) = &hair {
            // card id 0.5: the colour for viewers that cannot draw the hair shader
            mat.insert("color".into(), g4(&hairpbr_albedo(h, 0.5, 1.0)));
            let hair_op = s.texture("TexSlot1");
            let hair_id = if h.cards { s.texture("TexSlot4") } else { None };
            if self.lazy_textures {
                // the item pages evaluate the melanin / dye colour per pixel; alpha = OpacityMap.r
                let op = hair_op.map_or(-1, |o| self.texture(o, "or"));
                let id = hair_id.map_or(-1, |i| self.texture(i, ""));
                mat.insert(
                    "hair".into(),
                    json!({"mel": h.mel, "tint": h.tint, "mul": h.mul, "dye": h.dye, "dcol": h.dcol, "cards": h.cards, "op": op, "id": id}),
                );
            } else if self.with_textures {
                if let Some(o) = hair_op {
                    // single-asset page: the opacity map as an alpha-only texture, cut at 0.25 (CONFIRMED, HairPS)
                    let t = self.texture(o, "or");
                    mat.insert("tex".into(), json!(t));
                    if t >= 0 {
                        mat.insert("alpha_test".into(), json!(HAIR_ALPHA_TEST));
                    }
                }
            }
        }
        if s.alpha_test > 0.0 && tex >= 0 && s.opacity >= 0.99 && !CUTOUT_SKIP_SHADERS.contains(&sh) {
            if let Some(cov) = self.cutout_coverage(s.diffuse_tex.as_deref().unwrap_or(""), s.alpha_test) {
                mat.insert("acov".into(), json!(rnd(cov, 3)));
            }
        }
        if ALWAYS_BLENDED.contains(&sh) {
            mat.insert("trans".into(), json!(true)); // the zone viewer blends it whatever AlphaTest says (see is_translucent)
        }
        let fx = shader_fx(sh);
        if fx == Some("screen") || fx == Some("add") {
            mat.insert("fx".into(), json!(fx.unwrap()));
            if fx == Some("add") {
                mat.insert("trans".into(), json!(true));
            }
        }
        if is_rtt_text(s) {
            mat.insert("hide".into(), json!(true)); // see is_rtt_text
        }
        if sh == "GlassPBR" {
            // EMPIRICAL: 408 of ~7,600 sampled spaceship sub-materials are GlassPBR, every one with Opacity 1.0 and no diffuse
            // texture: the transparency is computed by the shader, not stored (CONFIRMED).  The real recipe is `gl` below; the flat
            // alpha is only the fallback for viewers without the glass program (a GUESS, not the engine's).
            if s.opacity > 0.99 {
                mat.insert("opacity".into(), json!(GLASS_ALPHA)); // fallback for viewers without the glass path (the single-asset page)
            }
            // CONFIRMED: the shader ignores the .mtl Opacity, so every GlassPBR gets the glass program (Opacity 0.84 etc. too)
            mat.insert("glass".into(), json!(true));
            if let Some(gl) = self.glass(s) {
                mat.insert("gl".into(), gl);
            }
        }
        if sh == "Decal" || sh == "WaterDecal" {
            // parameters of the projected-decal pixel shader (AlphaFalloff, AngleFadeThreshold)
            // TexSlot12 of decals is an opacity / mask map ('opac' in 67 of 182 Decal and every WaterDecal sampled); WaterDecal names
            // its angle fade NormalAngleFadeThreshold
            let op = s.texture("TexSlot12");
            let opac = match op {
                Some(o) if self.lazy_textures => self.texture(o, ""),
                _ => -1,
            };
            mat.insert(
                "decal".into(),
                json!({"falloff": num_param(&s.params, "AlphaFalloff", 1.0),
                       "angle": num_param(&s.params, "AngleFadeThreshold", num_param(&s.params, "NormalAngleFadeThreshold", 0.0)), "opac": opac}),
            );
        }
        // render-target screens: the viewer draws them (see `mfd`)
        let rk = mfd::rtt_kind(&s.shader, &s.textures);
        if rk == Some("screen") || rk == Some("hud") {
            mat.insert("rtt".into(), json!(rk.unwrap()));
            if let Some(asp) = mfd::screen_aspect(&s.textures) {
                if asp != 0.0 {
                    mat.insert("rasp".into(), json!(rnd(asp, 4)));
                }
            }
        }
        if ((rk == Some("screen") && sh == "DisplayScreen") || (rk == Some("hud") && sh == "UIPlane")) && gen_has(&s.gen_mask, "MONITOREFFECTS_HIGHTECH") {
            // the UIPlane (head-up plane) runs the same chain (UIPlanePS GL48, docs/material_coverage.md); its parameter names are taken to be the DisplayScreen ones (EMPIRICAL check: the test below)
            // %MONITOREFFECTS_HIGHTECH of the render-target screens (CONFIRMED, DisplayScreenPS GL4008 vs GL8): see `mfx` in zone.js
            let p = &s.params;
            // the interference / scanline maps are bound by the engine, not by the material (INFERRED from the names in ScreenInterference/: the
            // shader's InterferenceTexture is 256^2 vhs_noise_01, ScanlinesMap is scanlines_01; NoiseMap / BlocksMap only feed the damage glitch)
            let ia = num_param(&s.params, "InterferenceAmount", 0.0);
            let if_tex = if ia > 0.0 { self.texture("EngineAssets/Textures/ScreenInterference/vhs_noise_01.tif", "") } else { -1 };
            let sl_tex = if gen_has(&s.gen_mask, "MONITOREFFECTS_VIDEO") { self.texture("EngineAssets/Textures/ScreenInterference/scanlines_01.tif", "") } else { -1 };
            let grid = s.texture("TexSlot17").map(|g| self.texture(g, "")).unwrap_or(-1);
            mat.insert("mfx".into(), json!({"c": num_param(p, "EmissiveContrast", 1.0), "s": num_param(p, "EmissiveSaturation", 1.0), "va": num_param(p, "ViewingAngle", 120.0),
                "vm": num_param(p, "ViewAngleMode", 0.0), "gx": num_param(p, "PixelGridTilingX", 1.0), "gy": num_param(p, "PixelGridTilingY", 1.0), "g": grid,
                "ia": num_param(p, "InterferenceAmount", 0.0), "ib": num_param(p, "InterferenceBrightness", 1.0), "isp": num_param(p, "InterferenceSpeed", 0.0),
                "it": num_param(p, "InterferenceTiling", 1.0), "if": if_tex,
                "v": if gen_has(&s.gen_mask, "MONITOREFFECTS_VIDEO") { 1 } else { 0 }, "sl": sl_tex}));
        }
        if let Some(maps) = self.maps(s) {
            mat.insert("maps".into(), maps);
        }
        if let Some(tm) = self.texmods(s) {
            mat.insert("tm".into(), tm);
        }
        if self.with_textures && !self.lazy_textures {
            // embedded packed normal + gloss for the single-asset 3D page (zone / item pages use maps.norm / nrm instead)
            // MeshDecal keeps its ddna in slot 3
            let n = s.texture(if sh == "MeshDecal" { "TexSlot3" } else { "TexSlot2" });
            if let Some(n) = n.filter(|n| is_ddna(n) && (sh == "Illum" || sh == "MeshDecal")) {
                let ni = self.texture(n, "ng");
                if ni >= 0 {
                    mat.insert("nrm".into(), json!(ni));
                }
            }
        }
        let vm = vcol_mode(&s.shader, &s.gen_mask);
        if vm != 0 {
            mat.insert("vcol".into(), json!(true));
            mat.insert("vmode".into(), json!(vm));
            if gen_has(&s.gen_mask, "USE_DAMAGE_MAP") {
                mat.insert("damage_map".into(), json!(true));
            }
        }
        if let Some(mut md) = self.mesh_decal(s) {
            let st = if self.lazy_textures { self.stencil(s) } else { None };
            if let Some(st) = st {
                md["st"] = st;
            }
            mat.insert("md".into(), md);
        }
        let layered = if self.lazy_textures { self.layered(s) } else { None };
        if let Some(l) = layered {
            let cov = self.layered_cutout_coverage(s, &l);
            mat.insert("layered".into(), l);
            if let Some(cov) = cov {
                mat.insert("acov".into(), json!(rnd(cov, 3)));
            }
        }
        Value::Object(mat)
    }

    /// `acov` of a HardSurface material with AlphaTest > 0: the open fraction of its Primary layer's diffuse alpha (see
    /// cutout_coverage).
    ///
    /// The alpha source is INFERRED (not read from the shader): over the ship .mtl files, 544 of the 544 HardSurface sub-materials
    /// with AlphaTest > 0 whose Primary layer diffuse is BC3 have a partial alpha mask (0 < share >= AlphaTest < 1: anvl_grate_diff
    /// for Alpha_Tile_Grate*, the drak / krig / rsi `*_damage_*_diff` maps for Damage_Internal_*_Alpha, vanduul lattice maps), none a
    /// constant one, and the layer file itself carries the same AlphaTest (oval_tile_01.mtl 0.65 = its owner).  LayerBlend_V2 mixes
    /// four layers by the blend map: no single map has the coverage, so it gets none (the shader falls back to the plain test).
    fn layered_cutout_coverage(&mut self, s: &Sub, layered: &Value) -> Option<f64> {
        if layered["hs"] != Value::Bool(true) || s.alpha_test <= 0.0 || s.opacity < 0.99 {
            return None;
        }
        let tid = layered["layers"][0].get("tex").and_then(Value::as_i64).unwrap_or(-1);
        if tid < 0 || tid as usize >= self.textures.len() {
            return None;
        }
        let path = self.textures[tid as usize].get("path")?.as_str()?.to_string();
        self.cutout_coverage(&path, s.alpha_test)
    }

    /// GlassPBR parameters for the zone viewer's glass program.  From the GlassPBR_PS shader (CONFIRMED unless marked): the alpha is
    /// not the .mtl Opacity; Diffuse is an ABSORPTION colour (tint = Diffuse.rgb, times the vertex colour rgb with
    /// %PER_VERTEX_TINT), F0 = Specular.r (grey), normal map TexSlot2 (INFERRED slot) sampled at uv * GlassTiling with v stretched
    /// by GlassVStretch; transmittance Tr = (1 - fresnel) * exp(-0.1 (1/tint - 1) L).  The blend is dual-source (INFERRED); the
    /// viewer draws a multiplicative pass for Tr and an additive pass for the reflection.  Dirt: see `glass_dirt`.  Thin film (system
    /// texture, sc-45) and the tint palette (GL2000001 diff) are drawn.  Not drawn: refraction of the back buffer, emissive map / drop shadow, cracks, condensation, frost, decal, the tint palette and
    /// tint map.
    fn glass(&mut self, s: &Sub) -> Option<Value> {
        let p = &s.params;
        let n = s.texture("TexSlot2");
        let norm = match n {
            Some(n) if gen_has(&s.gen_mask, "NORMAL_MAP") && is_ddna(n) => self.texture(n, "ng"),
            _ => -1,
        };
        let mut out = json!({"tint": r4s(&s.diffuse), "f0": rnd(cl(s.specular[0], 0.0, 0.98), 4), "tiling": num_param(p, "GlassTiling", 1.0),
                             "vs": num_param(p, "GlassVStretch", 1.0), "norm": norm, "vtint": gen_has(&s.gen_mask, "PER_VERTEX_TINT")});
        if gen_has(&s.gen_mask, "USE_TINT_PALETTE") {
            // CONFIRMED (GlassPBR_PS GL2000001 vs GL1): with a palette assigned the diffuse is mix(MatDifColor, palette.Glass^2, GlassTintStrength)
            out["up"] = json!(true);
            out["gts"] = json!(num_param(p, "GlassTintStrength", 1.0));
        }
        if gen_has(&s.gen_mask, "THIN_FILM") {
            // CONFIRMED (sc-45): the gradient is the engine's system texture Engine/EngineAssets/Textures/thinfilm.dds, bound by no .mtl slot
            let t = self.texture("Engine/EngineAssets/Textures/thinfilm", "");
            if t >= 0 {
                out["film"] = json!({"t": t, "str": num_param(p, "ThinFilmStrength", 1.0), "th": num_param(p, "ThinFilmThickness", 1.0)});
            }
        }
        let dirt = if self.lazy_textures { self.glass_dirt(s) } else { None };
        if let Some(d) = dirt {
            out["dirt"] = d;
        }
        Some(out)
    }

    /// GlassPBR %DIRT layer for the zone viewer: {t: dirt texture id, tile, base, mask, fall} or None.
    ///
    /// CONFIRMED (GlassPBR_PS): dirt is per
    /// instance, the blend is `x = clamp((clamp(DirtBlendBase + (1-DirtBlendBase)*DIRT_AMT) - COLOR0.a*DirtBlendMask) /
    /// DirtBlendFalloff)`, d = x^2, the dirt texture is sampled at uv0 * DirtTiling and its alpha times d (times 2) is the opacity.
    /// TexSlot6 is NOT the mask, DirtReflectance / DirtSmoothness / DirtColour are not shader inputs (the
    /// colour is the per-instance dirt colour: white in the viewer).  EMPIRICAL: TexSlot11 is the dirt map in 1,787 of the 1,863
    /// ship glass sub-materials with %DIRT (the 76 without have nothing to draw).
    fn glass_dirt(&mut self, s: &Sub) -> Option<Value> {
        let dm = s.texture("TexSlot11")?;
        if !gen_has(&s.gen_mask, "DIRT") {
            return None;
        }
        let p = &s.params;
        let t = self.texture(dm, "");
        Some(json!({"t": t, "tile": num_param(p, "DirtTiling", 1.0), "base": num_param(p, "DirtBlendBase", 0.0), "mask": num_param(p, "DirtBlendMask", 1.0),
                    "fall": num_param(p, "DirtBlendFalloff", 1.0)}))
    }

    /// {d: TexSlot1 TexMod, n: TexSlot2 TexMod} (formats/mtl texmod, either key only when it does something) for the plain program,
    /// or None.
    ///
    /// The plain (non-layered) program samples the diffuse map with the TexSlot1 modifier and the packed normal / gloss / specular
    /// maps with the TexSlot2 one.  EMPIRICAL (Objects/Spaceships, props, UI .mtl files): TexSlot1 has Tile != 1 on ~2,700 Illum
    /// slots and an animated modifier (constant scroll / rotation) on ~1,900 slots of Illum, Monitor, UIMesh and Hologram.  The
    /// units and layout are CONFIRMED, the matrix order (tile, offset, then rotation about RotateCenter) and the animation types are
    /// INFERRED (see mtl::texmod).  Layered, glass and decal shaders have their own tilings and are left alone.
    fn texmods(&self, s: &Sub) -> Option<Value> {
        if ["LayerBlend_V2", "HardSurface", "GlassPBR", "MeshDecal", "Decal", "WaterDecal"].contains(&s.shader.as_str()) {
            return None;
        }
        let mut out = Map::new();
        for (k, slot) in [("d", "TexSlot1"), ("n", "TexSlot2")] {
            if let Some((_, t)) = s.texmods.iter().find(|(sl, _)| sl == slot) {
                out.insert(k.into(), texmod_value(t));
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(Value::Object(out))
        }
    }

    /// HardSurface %IRIDESCENCE: {t: gradient texture id, s: IridescenceStrength, u: ThicknessU, v: ThicknessV} or None.
    ///
    /// CONFIRMED (HardSurfacePS): the gradient is TexSlot10, the three parameters are PublicParams; the viewer applies the effect to
    /// the specular colour.  EMPIRICAL (Objects/Spaceships): 293 of 452 HardSurface %IRIDESCENCE sub-materials carry TexSlot10
    /// (`espr_irridescence_green_purple_diff`, `Vehicles_Irridescence_Gradiant__purple_blue` ...); the other 159 have no gradient in the
    /// layer files either (checked) and take the system default thinfilm.dds (INFERRED fallback, see below).  LayerBlend_V2 packs the
    /// same three values (2 ship sub-materials, no TexSlot10 either) and is not drawn.
    fn iridescence(&mut self, s: &Sub) -> Option<Value> {
        if !gen_has(&s.gen_mask, "IRIDESCENCE") {
            return None;
        }
        // a material without TexSlot10 gets the engine's default texture of slot type 10 (Iridescence), the system texture thinfilm.dds
        // (sc-45: GetDefaultTextureForSlotType case 10 -> the thinfilm global, CONFIRMED; that the loader falls back to it for a missing
        // slot is INFERRED from the function's shape, and 159 ship sub-materials have no gradient anywhere else)
        let g = s.texture("TexSlot10").unwrap_or("Engine/EngineAssets/Textures/thinfilm");
        let t = self.texture(g, "");
        Some(json!({"t": t, "s": num_param(&s.params, "IridescenceStrength", 1.0), "u": num_param(&s.params, "IridescenceThicknessU", 1.0),
                    "v": num_param(&s.params, "IridescenceThicknessV", 1.0)}))
    }

    /// Maps of a MeshDecal (hull panel lines, warning tape, screws).  Slots (CONFIRMED by the shader's
    /// resource set; the file-name suffixes agree on 6,797 sub-materials, EMPIRICAL): TexSlot2 SpecularTexture ('spec'), TexSlot3
    /// NormalTexture ('ddna', packed like the Illum one; its alpha companion is the gloss), TexSlot4 HeightTexture ('displ',
    /// %PARALLAX_OCCLUSION_MAPPING: the Illum ray march with PomDisplacement / PomHeightBias, uv shifted for every texture).  In the
    /// Gladius, Perseus and Cutlass .mtl files 50 of 184 sub-materials carry TexSlot1 diff + TexSlot3 ddna.  The decal is lit with
    /// its own normal map and alpha blended over the surface (MeshDecalPS writes a forward colour; there is no blend with the
    /// surface normal in the shader).  The specular colour is MatSpecColor times the map (used only when the map exists: a missing
    /// map's default is not known).
    fn decal_maps(&mut self, s: &Sub) -> Option<Value> {
        let mut out = Map::new();
        if let Some(n) = s.texture("TexSlot3").filter(|n| is_ddna(n)) {
            let v = self.texture(n, "ng");
            out.insert("norm".into(), json!(v));
        }
        if let Some(sp) = s.texture("TexSlot2").filter(|n| name_tail(n) == "spec") {
            let v = self.texture(sp, "");
            out.insert("spec".into(), json!(v));
        }
        let spec_ok = out.get("spec").and_then(Value::as_i64).unwrap_or(-1) >= 0;
        out.insert("specColor".into(), if spec_ok { g4(&s.specular) } else { json!([0.04, 0.04, 0.04]) });
        let mut has_pom = false;
        if let Some(h) = s.texture("TexSlot4") {
            if gen_has(&s.gen_mask, "PARALLAX_OCCLUSION_MAPPING") {
                let hi = self.texture(h, "");
                if hi >= 0 {
                    out.insert("pom".into(), json!({"h": hi, "h2": -1, "disp": num_param(&s.params, "PomDisplacement", 0.025), "bias": num_param(&s.params, "PomHeightBias", 0.5)}));
                    has_pom = true;
                }
            }
        }
        let any = ["norm", "spec"].iter().any(|k| out.get(*k).and_then(Value::as_i64).unwrap_or(-1) >= 0) || has_pom;
        if any {
            Some(Value::Object(out))
        } else {
            None
        }
    }

    /// Extra maps of an Illum material for the zone viewer: normal map (TexSlot2, 'ddna': BC5 normals), specular colour (TexSlot4)
    /// and gloss (the alpha companion file of the ddna).  EMPIRICAL from the file-name suffixes of the slots over 5,000 sampled
    /// materials: Illum TexSlot2 'ddna' (8,154 of 8,800), TexSlot4 'spec' (3,768 of 4,200); the engine's own slot table was not
    /// decoded.  Only Illum (the commonest shader) is covered so far.
    fn maps(&mut self, s: &Sub) -> Option<Value> {
        if self.lazy_textures && s.shader == "MeshDecal" {
            return self.decal_maps(s);
        }
        if !self.lazy_textures || (s.shader != "Illum" && shader_fx(&s.shader) != Some("illum")) {
            return None;
        }
        let mut out = Map::new();
        let (n, sp) = (s.texture("TexSlot2"), s.texture("TexSlot4"));
        if let Some(n) = n.filter(|n| is_ddna(n)) {
            let v = self.texture(n, "ng"); // normal + gloss in one image
            out.insert("norm".into(), json!(v));
        }
        if let Some(sp) = sp.filter(|n| name_tail(n) == "spec") {
            let v = self.texture(sp, "");
            out.insert("spec".into(), json!(v));
        }
        out.insert("specColor".into(), g4(&s.specular));
        let b2 = self.blend_layer(s);
        let dt = self.detail(s);
        let pm = self.illum_pom(s);
        let any = ["norm", "spec"].iter().any(|k| out.get(*k).and_then(Value::as_i64).unwrap_or(-1) >= 0) || b2.is_some() || dt.is_some() || pm.is_some();
        if let Some(b) = b2 {
            out.insert("layer2".into(), b);
        }
        if let Some(d) = dt {
            out.insert("detail".into(), d);
        }
        if let Some(p) = pm {
            out.insert("pom".into(), p);
        }
        if any {
            Some(Value::Object(out))
        } else {
            None
        }
    }

    /// Parallax occlusion mapping of an Illum material (%PARALLAX_OCCLUSION_MAPPING, 1,774 of 2,500 sampled POM sub-materials are
    /// Illum).  CONFIRMED (Illum pixel shader): the height field is the red channel of TexSlot8 (with
    /// BLENDLAYER mixed with TexSlot11 at uv * BlendLayer2Tiling by the blend weight); PomDisplacement is the uv shift per height
    /// unit, HeightBias (the Illum name of POMHeightBias) the height that does not move; the shifted uv replaces uv0 for every
    /// lookup.  The zone viewer does the ray march itself.
    fn illum_pom(&mut self, s: &Sub) -> Option<Value> {
        let t8 = s.texture("TexSlot8")?;
        if !gen_has(&s.gen_mask, "PARALLAX_OCCLUSION_MAPPING") {
            return None;
        }
        let h1 = self.texture(t8, "");
        if h1 < 0 {
            return None;
        }
        let h2 = match s.texture("TexSlot11") {
            Some(t) if gen_has(&s.gen_mask, "BLENDLAYER") => self.texture(t, ""),
            _ => -1,
        };
        Some(json!({"h": h1, "h2": h2, "disp": num_param(&s.params, "PomDisplacement", 0.025), "bias": num_param(&s.params, "HeightBias", 0.5)}))
    }

    /// Detail maps of an Illum material with %DETAIL_MAPPING (2,865 of 6,000 sampled material files).  From the Illum pixel
    /// shader (CONFIRMED unless marked): TexSlot6 is the first layer's detail map, TexSlot13 the second layer's; a
    /// texel is read as (G, A, R, B) * 2 - 1 = (normal x, normal y, diffuse, gloss).  The layer's diffuse is multiplied by 1 +
    /// diffuse * DetailDiffuseScale (BlendDetailDiffuseScale for the second layer), its gloss by 1 + gloss * DetailGlossScale, and
    /// its normal is combined with xy * DetailBumpScale by reoriented normal mapping (evaluated in the zone viewer).  The uv is uv0 times
    /// DetailTiling, which is not a PublicParam: INFERRED to be (TileU, TileV) of slot 6's TexMod and (TileU, TileV) of slot 13's (1
    /// without a TexMod); the texmod offset and rotation are not applied.
    fn detail(&mut self, s: &Sub) -> Option<Value> {
        if !gen_has(&s.gen_mask, "DETAIL_MAPPING") {
            return None;
        }
        let p = &s.params;
        let tile = |slot: &str| s.tiles.iter().find(|(k, _)| k == slot).map_or((1.0, 1.0), |(_, t)| *t);
        let i1 = match s.texture("TexSlot6") {
            Some(t) => self.texture(t, ""),
            None => -1,
        };
        let i2 = match s.texture("TexSlot13") {
            Some(t) if gen_has(&s.gen_mask, "BLENDLAYER") => self.texture(t, ""),
            _ => -1,
        };
        if i1 < 0 && i2 < 0 {
            return None;
        }
        let (t6, t13) = (tile("TexSlot6"), tile("TexSlot13"));
        Some(json!({"t1": i1, "t2": i2, "tile": [t6.0, t6.1, t13.0, t13.1],
                    "s1": [num_param(p, "DetailDiffuseScale", 0.0), num_param(p, "DetailBumpScale", 0.0), num_param(p, "DetailGlossScale", 0.0)],
                    "s2": [num_param(p, "BlendDetailDiffuseScale", 0.0), num_param(p, "BlendDetailBumpScale", 0.0), num_param(p, "BlendDetailGlossScale", 0.0)]}))
    }

    /// The second layer of an Illum material with %BLENDLAYER (3,957 of 6,000 sampled material files have sub-materials with it).
    /// EMPIRICAL (file-name suffixes and parameter names, every one of them present on all 3,957): TexSlot9 second diffuse,
    /// TexSlot3 second ddna, TexSlot10 second specular, TexSlot12 blend mask; PublicParams BlendLayer2DiffuseColor / SpecularColor
    /// / Glossiness (0..255) / Tiling, BlendMaskTiling, BlendFactor, BlendFalloff.  The blend rule itself is evaluated in the viewer
    /// and is INFERRED, not confirmed.
    fn blend_layer(&mut self, s: &Sub) -> Option<Value> {
        if !gen_has(&s.gen_mask, "BLENDLAYER") {
            return None;
        }
        let p = &s.params;
        let (mask, d2) = (s.texture("TexSlot12")?, s.texture("TexSlot9")?);
        let (n2, sp2) = (s.texture("TexSlot3"), s.texture("TexSlot10"));
        let is_n = n2.map_or(false, is_ddna);
        let mask_i = self.texture(mask, "");
        let tex_i = self.texture(d2, "");
        let ng = if is_n { self.texture(n2.unwrap(), "ng") } else { -1 };
        let spec = match sp2 {
            Some(t) => self.texture(t, ""),
            None => -1,
        };
        Some(json!({"mask": mask_i, "tex": tex_i, "ng": ng, "spec": spec,
                    "color": g4(&vec3(p, "BlendLayer2DiffuseColor").unwrap_or([1.0; 3])),
                    "specColor": g4(&vec3(p, "BlendLayer2SpecularColor").unwrap_or([0.04; 3])),
                    "gloss": rnd((num_param(p, "BlendLayer2Glossiness", 128.0) / 255.0).min(1.0), 3),
                    "tiling": num_param(p, "BlendLayer2Tiling", 1.0), "mtiling": num_param(p, "BlendMaskTiling", 1.0),
                    "factor": num_param(p, "BlendFactor", 1.0), "falloff": num_param(p, "BlendFalloff", 1.0)}))
    }

    /// Mesh decals.  Two shaders draw them, both forward alpha-blended geometry lying on the surface it decorates (a clip-space z
    /// bias):
    ///
    /// * `Shader="MeshDecal"` (k=1).  MeshDecalPS (CONFIRMED): a = diffuse.a [* COLOR0.a with
    ///   VERTCOLORS]; with AlphaGlowCutoff c != 0: opacity = clamp(a / c) and glow mask = clamp((a - c) / (1 - c)), else both = a;
    ///   opacity = clamp(pow(clamp(opacity * DecalAlphaMultiplier), DecalFalloff)); output alpha = opacity * MatDifColor.a^2 *
    ///   instance opacity (MatDifColor.a, here the material Opacity, enters twice), NOT premultiplied (SRC_ALPHA /
    ///   ONE_MINUS_SRC_ALPHA; the engine's blend state itself was not decoded: INFERRED); emission = diffuse * glow mask [* COLOR0.g
    ///   with VERTDATA] * (MatEmissiveColor.a + glow from the instance).
    /// * Illum with `%DECAL` (k=0; EMPIRICAL: 13,951 sub-materials, e.g. lowtech 'pom_decals', the Mole 'decals').
    ///   IllumDecalPS (CONFIRMED): alpha = clamp(pow(diffuse.a * DecalOpacityMap.r *
    ///   DecalAlphaMult, DecalFalloff)) * instance opacity * MatDifColor.a [* COLOR0.a with VERTCOLORS]; the map is TexSlot12 (`op`;
    ///   only 460 of 7,577 %DECAL_OPACITY_MAP materials name one, the others take 1: the engine's default for an empty slot is
    ///   unknown); NO alpha test (only the %USE_DAMAGE_MAP permutations kill, on the damage threshold); the glow factor is
    ///   clamp((alpha - AlphaGlowCutoff) / (1 - AlphaGlowCutoff)) and the emission albedo * factor * MatEmissiveColor.a.
    ///
    /// `glow` = MatEmissiveColor.a (CONFIRMED against the engine's material loader): (20 g)^2 with g = GlowAmount * 0.1 when the material has GlowAmount, else the raw `Glow` attribute (GlowAmount 1
    /// -> 4, Glow 1 -> 400).  The .mtl Emissive rgb is copied into MatEmissiveColor.rgb but neither decal shader reads it: the glow
    /// colour is the diffuse.  The viewers tone-map the HDR value.
    ///
    /// MeshDecal only (MeshDecalPS, CONFIRMED): `tm` = ETintModeType (palette index rule: 0 untinted, 1..3 entry, 4 entry 3 or COLOR0.b with VERTDATA, 5 / 6 uv tile cells), `tmt` =
    /// TintModeTexture (TexSlot6: r = diffuse tint amount, g = specular tint amount, b = darkening of negative cells; an empty slot
    /// is white), `wear` = {base: WearBlendBase, falloff: WearBlendFalloff, tile: WearTiling} with `bl` = BlendTexture
    /// (TexSlot5): the decal alpha is multiplied by the wear mask (1 with no wear: 2,978 of 3,002 sampled ship MeshDecals
    /// have WearBlendBase 0); `vdata` = %VERTDATA (COLOR0 is data).
    ///
    /// %DISABLE_ALPHA_BLEND only changes the CPU-side blend state (the instruction streams are identical); the viewer draws
    /// such a decal in the opaque pass and drops its fully transparent texels (INFERRED stand-in for the unknown state).  `v` is the
    /// variant key a decal census counts.  `nobase`: a MeshDecal with %STENCIL_MAP and no diffuse map or texture has no base
    /// layer (INFERRED alpha 0), only its stencil (the palette's decal texture, see `stencil`) can show.  Not drawn: the POM
    /// permutations' wear mix toward the WearDiffuse / WearSpecular textures (slots not decoded), StencilBreakup / edge wear of
    /// MeshDecal stencils, instance glow.
    fn mesh_decal(&mut self, s: &Sub) -> Option<Value> {
        let mesh = s.shader == "MeshDecal";
        if !mesh && !gen_has(&s.gen_mask, "DECAL") {
            return None;
        }
        let p = &s.params;
        let num = |keys: &[&str], default: f64| -> f64 { keys.iter().find_map(|k| param(p, k).and_then(py_float)).unwrap_or(default) };
        let mut out = Map::new();
        out.insert("mult".into(), json!(num(&["DecalAlphaMultiplier", "DecalAlphaMult"], 1.0)));
        out.insert("falloff".into(), json!(num(&["DecalFalloff"], 1.0)));
        out.insert("cut".into(), json!(num(&["AlphaGlowCutoff"], 0.0)));
        out.insert("blend".into(), json!(!gen_has(&s.gen_mask, "DISABLE_ALPHA_BLEND")));
        out.insert("k".into(), json!(if mesh { 1 } else { 0 }));
        out.insert("v".into(), json!(decal_variant(&s.shader, &s.gen_mask)));
        let mut ga = s.glow_amount;
        if ga.is_none() && param(p, "GlowAmount").is_some() {
            ga = Some(num(&["GlowAmount"], 0.0));
        }
        let g = match ga {
            Some(ga) => ga * 0.1,
            None => s.glow,
        };
        if g > 0.0 {
            out.insert("glow".into(), json!(rnd((20.0 * g).powi(2), 4)));
        }
        if mesh && gen_has(&s.gen_mask, "STENCIL_MAP") && s.diffuse_tex.is_none() && !gen_has(&s.gen_mask, "DIFFUSE_MAP") {
            out.insert("nobase".into(), json!(true));
        }
        if mesh {
            let tm = num(&["ETintModeType"], 0.0) as i64;
            if tm != 0 {
                out.insert("tm".into(), json!(tm));
            }
            if gen_has(&s.gen_mask, "VERTDATA") {
                out.insert("vdata".into(), json!(true));
            }
            let base = num(&["WearBlendBase"], 0.0);
            out.insert("wear".into(), json!({"base": base, "falloff": num(&["WearBlendFalloff"], 1.0), "tile": num(&["WearTiling"], 1.0)}));
            if self.lazy_textures {
                if let Some(b) = s.texture("TexSlot5") {
                    let v = self.texture(b, "");
                    out.insert("bl".into(), json!(v));
                }
                if let (true, Some(t)) = (tm != 0, s.texture("TexSlot6")) {
                    let v = self.texture(t, "");
                    out.insert("tmt".into(), json!(v));
                }
            }
        } else if gen_has(&s.gen_mask, "DECAL_OPACITY_MAP") && self.lazy_textures {
            if let Some(o) = s.texture("TexSlot12") {
                let v = self.texture(o, "");
                out.insert("op".into(), json!(v));
            }
        }
        Some(Value::Object(out))
    }

    /// %STENCIL_MAP of a MeshDecal (1,893 sub-materials: ship branding, numbers and registration marks, EMPIRICAL).
    /// CONFIRMED (the HardSurface stencil function, shared by MeshDecalPS): the stencil map (TexSlot7) is read at
    /// uv * StencilTiling; each of its three channels is thresholded at 0.5 (anti-aliased by fwidth) into exclusive weights r, g, b
    /// (g only where r is off, ...) times StencilOpacity; the colour of channel i is StencilDiffuseColor / 2 / 3
    /// (StencilTintOverride 0), the palette's entry tints (1) or the palette's decalColorR / G / B (2); total = the sum of the
    /// weights and the output alpha is alpha * (1 - total) + total.  `$TintPaletteDecal` in TexSlot7 means the map is the palette's
    /// own decalTexture (916 of 2,423 palettes have one: ship logos); the viewer takes it from the palette JSON.
    /// The colour joins the diffuse as diffuse * (1 - total) + sum(weight * colour): CONFIRMED in the LayerBlend_V2 copy of the
    /// function, INFERRED to be the same in MeshDecalPS (the shared stencil function); the output stays non-premultiplied.  The StencilBreakup / edge wear (wear 0 statically)
    /// and the StencilGlowAmt glow mix are not drawn.  None without the flag.
    fn stencil(&mut self, s: &Sub) -> Option<Value> {
        if !gen_has(&s.gen_mask, "STENCIL_MAP") {
            return None;
        }
        let p = &s.params;
        let col = |k: &str| -> Value { vec3(p, k).map_or(json!([1.0, 1.0, 1.0]), |v| g4(&v)) };
        let src = s.texture("TexSlot7").unwrap_or("");
        let pal = src.to_lowercase().starts_with("$tintpalettedecal");
        let tid = if pal || src.is_empty() { -1 } else { self.texture(src, "") };
        if !pal && tid < 0 {
            return None;
        }
        Some(json!({"pal": pal, "tex": tid, "tile": num_param(p, "StencilTiling", 1.0), "op": num_param(p, "StencilOpacity", 1.0),
                    "tint": num_param(p, "StencilTintOverride", 0.0) as i64,
                    "col": [col("StencilDiffuseColor"), col("StencilDiffuseColor2"), col("StencilDiffuseColor3")]}))
    }

    /// HardSurface %MACRO_EMISSIVE_MAP: {t: TexSlot14 texture id, tile: EmissiveTiling} or None.
    ///
    /// EMPIRICAL: TexSlot14 exists exactly when %MACRO_EMISSIVE_MAP (9,840 sampled HardSurface sub-materials, see `layered`); its
    /// files are glow masks ('_glow', '_lse', 'thruster_glow_lse_diff': bright where the surface lights up, black elsewhere) and
    /// every such material carries EmissiveTiling (and FarGlow* distances).  The pixel shader's resource is named MacroEmissiveMap
    /// (CONFIRMED).  CONFIRMED: the colour is EmissiveArrayMap * EmissiveMap * MatEmissiveColor.rgb times (instanceGlow *
    /// GlowFactor + MatEmissiveColor.a), times vertex green with %VERTCOLORS; the intensity and far-glow ramp are in `emission`,
    /// the map is sampled at uv * EmissiveTiling (INFERRED).  EmissiveArrayMap is not drawn (no slot known).
    fn macro_emissive(&mut self, s: &Sub) -> Option<Value> {
        let src = s.texture("TexSlot14")?;
        if !gen_has(&s.gen_mask, "MACRO_EMISSIVE_MAP") || !self.lazy_textures {
            return None;
        }
        let ti = self.texture(src, "");
        if ti < 0 {
            return None;
        }
        let tile = por(&s.params, "EmissiveTiling", 1.0).unwrap_or(1.0);
        Some(json!({"t": ti, "tile": tile}))
    }

    /// A HardSurface sub-material with NO <MatLayers> but a driven macro emissive map (`Glow_Thrusters` of drak_int_master_01: a
    /// thruster glow strip that carries only TexSlot14 + Emissive + Glow): drawn as the layered program with ONE default layer and
    /// the macro emissive map (`em`), or None.
    ///
    /// EMPIRICAL (all 2,591 Objects/Spaceships .mtl files): 111 HardSurface / LayerBlend_V2 sub-materials have no layers; 78 have no
    /// texture at all, 13 more only an unused map (the MOLE paint tile, a quantum-drive hue map) and exactly ONE (`Glow_Thrusters`:
    /// Emissive 0.095,0.023,0.004, Glow 1, %MACRO_EMISSIVE_MAP, Opacity 1) is an emitter.  The others have no emission and no
    /// texture and stay on the plain program (which draws MatDifColor, the same as the default layer below).  CONFIRMED: a
    /// layerless LayerBlend uses the default SMaterialLayer (diffuse tint white x MatDifColor, specular 0, gloss 0, texture ids 0),
    /// NOT black; what it shows depends on texture-library slot 0 (unchecked, so the viewer uses a plain colour).  The layerless
    /// powr_just_s03_pl01/02/03 'screen' sub-materials have Glow 0 and Emissive 0: they emit nothing and draw as the default layer.
    /// HardSurface layerless (119 of 23,770): the builder was not traced, the same default layer is INFERRED.  This replaces the
    /// earlier INFERRED 'black base'.
    fn layerless_glow(&mut self, s: &Sub) -> Option<Value> {
        let em = emission(s);
        if s.shader != "HardSurface" || !self.lazy_textures || em.as_ref().map_or(true, |e| e.kind != 'm') {
            return None;
        }
        let macro_ = self.macro_emissive(s)?;
        let base = json!({"tint": g4(&s.diffuse), "tiling": 1.0, "pal": 0, "pd": 0, "ps": 0, "tex": -1, "ng": -1, "spec": [0.0, 0.0, 0.0], "gloss": 0.0});
        Some(json!({"layers": [base, null, null, null], "blend": -1, "btile": 1.0, "hal": -1, "norm": -1, "rot": false, "wear": null, "hs": true, "mtile": 1.0,
                    "vcol": gen_has(&s.gen_mask, "VERTCOLORS"), "pom": null, "em": macro_}))
    }

    /// Specular and detail maps of a HardSurface material's Primary layer file `f` (a Shader="Layer" sub-material), as extra keys
    /// of its layer description: `sm` = specular map id, `dt` = {t: detail map id, tile: DetailTiling, s: [DetailDiffuse,
    /// DetailBump, DetailGloss]}.  Empty when neither applies.
    ///
    /// Slots, EMPIRICAL (3,000 random layer .mtl files, file-name suffixes): TexSlot6 'spec' (292; TexSlot6 is 'diff' twice),
    /// TexSlot7 'detail' / 'udm' / 'tdm' (~370), with the PublicParams DetailTiling / DetailDiffuse / DetailBump / DetailGloss on
    /// the same files.  Gating, INFERRED from the permutation token names: the owner needs %USE_SPECULAR_MAPS for the specular map
    /// and %USE_DETAIL_MAPS for the detail map (on the Ironclad 2.2 M of 12.4 M triangles with a layer spec map lack the first
    /// token: those stay on the layer's Specular colour).  How they are applied, INFERRED (not yet confirmed): the specular
    /// colour is the layer Specular times the map (104 of 150 sampled layers with a map have Specular 1.0, so the map is the F0
    /// colour); the detail texel is decoded like the Illum one (CONFIRMED there: (G, A, R, B) * 2 - 1 = bump x, bump y,
    /// diffuse, gloss; EMPIRICAL on ship_hf_paintudm_b and metal_scratches-01_detail: G and A average 127.5) at uv * the layer
    /// tiling * DetailTiling (an assumption: the engine packs DetailTiling only in the LOW half of the packed Tiling_DetailTiling and
    /// the blend maps in the high half, LayerTiling * UVTiling; which half the detail map samples is not traced), diffuse *= 1 + d *
    /// DetailDiffuse, gloss *= 1 + d * DetailGloss, normal += bump xy * DetailBump.
    fn hs_layer_maps(&mut self, s: &Sub, f: Option<&Sub>) -> Map<String, Value> {
        let mut out = Map::new();
        let Some(f) = f else { return out };
        if !self.lazy_textures {
            return out;
        }
        let fp = &f.params;
        if let Some(sp) = f.texture("TexSlot6") {
            if gen_has(&s.gen_mask, "USE_SPECULAR_MAPS") && suffix(sp) != "diff" {
                let si = self.texture(sp, "");
                if si >= 0 {
                    out.insert("sm".into(), json!(si));
                }
            }
        }
        if let Some(dt) = f.texture("TexSlot7") {
            if gen_has(&s.gen_mask, "USE_DETAIL_MAPS") {
                let di = self.texture(dt, "");
                if di >= 0 {
                    out.insert(
                        "dt".into(),
                        json!({"t": di, "tile": num_param(fp, "DetailTiling", 1.0),
                               "s": [num_param(fp, "DetailDiffuse", 0.0), num_param(fp, "DetailBump", 0.0), num_param(fp, "DetailGloss", 0.0)]}),
                    );
                }
            }
        }
        out
    }

    /// Sub-material of a layer .mtl (the named sub-material, else the first), or None.
    fn layer_file(&mut self, path: &str, submtl: &str) -> Option<SubMaterial> {
        let p = path.replace('\\', "/");
        let stem = if p.to_lowercase().ends_with(".mtl") { p[..p.len() - 4].to_string() } else { p };
        let subs = self.load_mtl(&stem)?;
        if subs.is_empty() {
            return None;
        }
        Some(subs.iter().find(|x| !submtl.is_empty() && x.name == submtl).unwrap_or(&subs[0]).clone())
    }

    /// Description of one layer entry (a base or a wear layer) of a layered material, or None.
    fn layer_entry(&mut self, s: &Sub, l: Option<&Layer>) -> Option<Value> {
        let l = l?;
        if l.path.is_empty() {
            return None;
        }
        let f = self.layer_file(&l.path, &l.submtl);
        let empty: Vec<(String, String)> = vec![];
        let fp = f.as_ref().map_or(&empty, |f| &f.params);
        let (tiling, mode) = match (por(fp, "LayerTiling", 1.0), por(fp, "TintMode", 0.0)) {
            (Ok(t), Ok(m)) if m.is_finite() => (t * l.tiling, m as i64),
            _ => (l.tiling, 0),
        };
        let mut tint = [1.0, 1.0, 1.0];
        if mode == 0 || mode == 1 {
            let d = f.as_ref().map_or([1.0; 3], |f| f.diffuse);
            for k in 0..3 {
                tint[k] = l.tint[k] * d[k] * s.diffuse[k]; // owner Diffuse too (CONFIRMED)
            }
        }
        let tex = f.as_ref().and_then(|f| f.texture("TexSlot1")).map(str::to_string);
        let nrm = f.as_ref().and_then(|f| f.texture("TexSlot2")).map(str::to_string);
        let ng = match &nrm {
            Some(n) if is_ddna(n) => self.texture(n, "ng"),
            _ => -1,
        };
        // EMPIRICAL guess: the layer's glossiness is its material Shininess / 255 (the layer files carry Shininess 160 and no
        // Glossiness attribute), multiplied by the entry's GlossMult (CONFIRMED)
        let gloss = match &f {
            Some(f) if f.shininess != 0.0 => f.shininess / 255.0,
            _ => 0.5,
        } * l.gloss_mult;
        // PaletteTint (0..3): which palette entry tints the layer (CONFIRMED).  Whether the diffuse and / or the specular is tinted
        // follows the layer's TintMode, which is INFERRED (0 = both, 1 = diffuse, 2 = specular)
        let tex_i = match &tex {
            Some(t) => self.texture(t, ""),
            None => -1,
        };
        Some(json!({"tint": g4(&tint), "tiling": rnd(tiling, 4), "pal": l.palette_tint,
                    "pd": if mode == 0 || mode == 1 { 1 } else { 0 }, "ps": if mode == 0 || mode == 2 { 1 } else { 0 },
                    "tex": tex_i, "ng": ng,
                    "spec": f.as_ref().map_or(json!([0.04, 0.04, 0.04]), |f| g4(&f.specular)),
                    "gloss": rnd(gloss.min(1.0), 3)}))
    }

    /// Description of a LayerBlend_V2 / HardSurface material for the zone viewer, or None.
    ///
    /// What the viewer evaluates (from the shader SPIR-V in the shader cache pak; C = confirmed there, I = inferred):
    ///   * LayerBlend_V2: BlendMap (TexSlot12, sampled at UV0 * BlendMapTiling) gives hierarchical weights L1 = r, L2 = g(1-r), L3 =
    ///     b(1-r)(1-g), L4 = (1-r)(1-g)(1-b) (C); without a blend map only layer 1 (I).  Wear layers are left out: with no wear
    ///     state the wear weight is 0 (C).  TexSlot13 is HueAoLumMap (C): .y ambient occlusion and .z luminance (colour *= 0.8 + 0.4
    ///     z) are applied, .x (hue LUT) is not.
    ///   * HardSurface: with no wear amount the Primary layer alone shows (C).
    ///   * layer UV = UV0 * (layer file LayerTiling * material UVTiling) (C), (u, v) -> (v, -u) when RotateLayerUVs.
    ///   * TintMode 0 and 1: diffuse = TintColor * layer Diffuse * texture; 2 and above: untinted diffuse (C).  PaletteTint needs
    ///     an entity tint palette, which a bare model does not have: ignored (C).
    ///   * TexSlot3 is the normal map (DDNA); its tangent frame is derived in the viewer from UV derivatives and the green-channel
    ///     direction is a guess (I).
    /// Also evaluated now: per-layer normal maps (layer TexSlot2) and gloss (its alpha companion), the layer's Specular colour, the
    /// owner material's Diffuse as a multiplier on each layer's diffuse (C).
    /// Wear: WearLayer1..4 (their own TintColor / tiling / diffuse) and the wear map (TexSlot11) are sent; the viewer blends them in
    /// by a global wear amount (default 0).  The shader: base weight w(1-wf), wear weight w*wf, wf from the wear map's red channel, the
    /// instance wear amount and the layer's falloff; the viewer's wf = wear * (1 - wm^3) is an APPROXIMATION of that.  Not
    /// evaluated: dirt (per-object state), iridescence.
    /// Wear amount per shader (CONFIRMED in the SPIR-V of Engine/ShaderCache_Vulkan.pak): LayerBlend_V2 amount =
    /// clamp(instance wear byte 0 / 255) ONLY: no WearBlendBase, no vertex term, the mask is WearMap.r, wear 0 gives mask exactly 1
    /// for any blend value; the %WEAR_DIRT_AO_VERTEX flag is a no-op there (no vertex wear / dirt / AO channels exist for
    /// LayerBlend: vertex alpha = 255 - sub-material index (min 32), with DECALS the rgb is a packed UV), so the LayerBlend wear
    /// dict carries no `base` and the viewer's LayerBlend branch ignores vertex colour.  HardSurface amount = clamp(max(WearBlendBase,
    /// instance wear) - 0.95 * COLOR0.a [VERTCOLORS]); instance wear = byte 0 of instance dword 2 / 255 (+ byte 2 *
    /// DamagePerObjectWear with USE_DAMAGE_MAP: not modelled).  With no WearMap and wear > 0 the mask depends on the engine's
    /// default texture: INFERRED white (mask 1, the material never wears), the viewer still uses wm = 0.5 there (unchanged, no
    /// survey evidence).  Per-item default wear: writer not located, INFERRED 0 (the viewer's wear slider default).  HardSurface has
    /// no DirtColor member: dirt colour comes only from the per-instance dirtColor, which the viewer does not have, so dirt stays
    /// off.  Palette tints apply to each layer (base AND wear) with that layer's own PaletteTint (C: one LayerDefinition per
    /// mtl layer, so a wear layer has its own palette index and flags).
    pub fn layered(&mut self, s: &Sub) -> Option<Value> {
        let shader = s.shader.as_str();
        if shader != "LayerBlend_V2" && shader != "HardSurface" {
            return None;
        }
        if s.layers.is_empty() {
            return self.layerless_glow(s);
        }
        let lb = shader == "LayerBlend_V2";
        let names: &[&str] = if lb { &["BaseLayer1", "BaseLayer2", "BaseLayer3", "BaseLayer4"] } else { &["Primary"] };
        let mut by_name: HashMap<&str, &Layer> = HashMap::new();
        for l in &s.layers {
            by_name.insert(l.name.as_str(), l);
        }
        let p = &s.params;
        let rot = matches!(param(p, "RotateLayerUVs"), Some("1") | Some("true"));

        let mut layers: Vec<Value> = vec![];
        for nm in names {
            layers.push(self.layer_entry(s, by_name.get(nm).copied()).unwrap_or(Value::Null));
        }
        if !lb && !layers[0].is_null() {
            let prim = by_name["Primary"];
            let pf = self.layer_file(&prim.path, &prim.submtl);
            let extra = self.hs_layer_maps(s, pf.as_ref());
            if let Value::Object(m) = &mut layers[0] {
                m.extend(extra);
            }
        }
        let mut wear = Value::Null;
        if lb {
            let mut wl = vec![];
            for k in 1..=4 {
                wl.push(self.layer_entry(s, by_name.get(format!("WearLayer{}", k).as_str()).copied()).unwrap_or(Value::Null));
            }
            if wl.iter().any(|w| !w.is_null()) {
                let wm = match s.texture("TexSlot11") {
                    Some(t) => self.texture(t, ""),
                    None => -1,
                };
                wear = json!({"map": wm, "layers": wl});
            }
        } else {
            // HardSurface Primary <-> Wear (HardSurface pixel shader, C): wearAmount = max(WearBlendBase, instance wear) - 0.95
            // * vertex colour alpha; the Primary layer weighs 1 - wf and the Wear layer wf.  Not evaluated: the falloff /
            // ShadingModel remap of wf and the Primary layer's blend map as a mask (the layer file slot is unresolved), so the
            // viewer uses wf = wearAmount, and only above a wear slider of 0.
            if let Some(wl) = self.layer_entry(s, by_name.get("Wear").copied()) {
                let base = por(p, "WearBlendBase", 0.0).unwrap_or(0.0);
                let mut w = json!({"map": -1, "layers": [wl, null, null, null], "hs": true, "base": base});
                let prim = by_name.get("Primary").copied();
                let pf = match prim {
                    Some(pr) if !pr.path.is_empty() => self.layer_file(&pr.path, &pr.submtl),
                    _ => None,
                };
                if let (Some(pf), Some(prim)) = (&pf, prim) {
                    // Primary_Blend = TexSlot8 of the Primary LAYER file, sampled at uv0 * the HIGH half of its packed
                    // Tiling_DetailTiling (CONFIRMED in HardSurfacePS for the sampling; the CPU packs the high half as LayerTiling *
                    // UVTiling and DetailTiling only into the low half, CONFIRMED in the engine's layer packer; that the
                    // HardSurface builder uses the same packer is INFERRED) (LayerTiling * UVTiling; NOT times DetailTiling, which
                    // lives only in the low half); WearDirtBlendFalloff and ShadingModel are the layer file's params (a byte each in
                    // the packed definition).  Without a blend map (13% of layers) the mask is unresolved: wf = wear amount.
                    let fp = &pf.params;
                    let bm = pf.texture("TexSlot8");
                    let blend = match bm {
                        Some(b) => self.texture(b, ""),
                        None => -1,
                    };
                    w["hsb"] = json!({"blend": blend, "tile": rnd(num_param(fp, "LayerTiling", 1.0) * prim.tiling, 4),
                                      "falloff": cl(num_param(fp, "WearDirtBlendFalloff", 1.0), 0.0, 1.0), "sm": num_param(fp, "ShadingModel", 0.0) as i64});
                }
                wear = w;
            }
        }
        let sec = if !lb { self.hs_secondary(s, &by_name, &mut layers) } else { None };
        if layers.iter().all(Value::is_null) {
            return None;
        }

        let btile = por(p, "BlendMapTiling", 1.0).unwrap_or(1.0);
        let hal_path = if lb { s.texture("TexSlot13") } else { None };
        let mut hal = if lb {
            match s.texture("TexSlot13") {
                Some(t) => self.texture(t, ""),
                None => -1,
            }
        } else {
            -1
        };
        let mut norm = match s.texture("TexSlot3") {
            Some(t) if lb => self.texture(t, "ng"),
            _ => -1,
        };
        let (mut mtile, mut em) = (1.0, None);
        if !lb {
            // HardSurface macro maps.  EMPIRICAL (9,840 sampled sub-materials): TexSlot3 exists exactly when %MACRO_NORMAL_MAP (a
            // ddn / ddna normal map, 1,380 of 1,380), TexSlot1 exactly when %MACRO_AO_MAP (near-white grey: paint_largescale_diff,
            // *_ls_ao), TexSlot6 exactly when %MACRO_HEIGHT_MAP, TexSlot7 when %STENCIL_MAP, TexSlot14 when %MACRO_EMISSIVE_MAP; the
            // pixel shader's resources are named MacroNormalMap / MacroAOMap / MacroHeightMap / StencilMap / MacroEmissiveMap
            // (CONFIRMED).  Drawn (CONFIRMED): the normal map (the blended layer normal reoriented onto it; its gloss companion
            // multiplies the gloss) and the AO map (.r multiplies the diffuse AND the specular; carried in the hue/AO/luminance slot
            // with a neutral hue and luminance), both at uv * MacroTiling.  The vertex colour blue (a second AO factor) is not
            // applied: the vertex colour byte order is unconfirmed.  The macro height map is the POM height when
            // %MACRO_HEIGHT_MAP.  Not drawn: stencil.
            if let (true, Some(t)) = (gen_has(&s.gen_mask, "MACRO_NORMAL_MAP"), s.texture("TexSlot3")) {
                norm = self.texture(t, "ng");
            }
            if let (true, Some(t)) = (gen_has(&s.gen_mask, "MACRO_AO_MAP"), s.texture("TexSlot1")) {
                hal = self.texture(t, "ao");
            }
            mtile = por(p, "MacroTiling", 1.0).unwrap_or(1.0);
            em = self.macro_emissive(s);
        }
        if wear["map"].as_i64().map_or(false, |m| m >= 0) {
            // the wear map (TexSlot11, UV0 untiled like the hue/AO/luminance map) rides in the alpha of the packed hal image
            hal = self.texture(hal_path.unwrap_or(""), &format!("hw:{}", s.texture("TexSlot11").unwrap_or("")));
            wear["map"] = json!(hal);
        }
        let mut pom = Value::Null;
        if !lb && gen_has(&s.gen_mask, "PARALLAX_OCCLUSION_MAPPING") {
            // HardSurface POM (CONFIRMED): height = the Primary layer's height (its layer file TexSlot3, red channel, at
            // the layer tiling) or, with %MACRO_HEIGHT_MAP, the macro height map (TexSlot6 at MacroTiling); PomDisplacement,
            // POMHeightBias.
            let prim = by_name.get("Primary").copied();
            let pf = match prim {
                Some(pr) if !pr.path.is_empty() => self.layer_file(&pr.path, &pr.submtl),
                _ => None,
            };
            let (h, ht);
            if let (true, Some(t6)) = (gen_has(&s.gen_mask, "MACRO_HEIGHT_MAP"), s.texture("TexSlot6")) {
                h = self.texture(t6, "");
                ht = mtile;
            } else if let Some((pf, prim)) = pf.as_ref().zip(prim).filter(|(pf, _)| pf.texture("TexSlot3").is_some()) {
                let t3 = pf.texture("TexSlot3").unwrap().to_string();
                ht = match (por(&pf.params, "LayerTiling", 1.0), por(&pf.params, "DetailTiling", 1.0)) {
                    (Ok(a), Ok(b)) => a * prim.tiling * b,
                    _ => prim.tiling,
                };
                h = self.texture(&t3, "");
            } else {
                h = -1;
                ht = 1.0;
            }
            if h >= 0 {
                pom = json!({"h": h, "tile": rnd(ht, 4), "disp": num_param(p, "PomDisplacement", 0.025), "bias": num_param(p, "POMHeightBias", 0.5)});
            }
        }
        let blend = if lb && s.gen_mask.contains("BLEND_MAP") {
            match s.texture("TexSlot12") {
                Some(t) => self.texture(t, ""),
                None => -1,
            }
        } else {
            -1
        };
        let mut out = Map::new();
        out.insert("layers".into(), Value::Array(layers));
        out.insert("blend".into(), json!(blend));
        out.insert("btile".into(), json!(btile));
        out.insert("hal".into(), json!(hal));
        out.insert("norm".into(), json!(norm));
        out.insert("rot".into(), json!(rot));
        out.insert("wear".into(), wear);
        out.insert("hs".into(), json!(!lb));
        out.insert("mtile".into(), json!(mtile));
        out.insert("vcol".into(), json!(gen_has(&s.gen_mask, "VERTCOLORS")));
        out.insert("pom".into(), pom);
        if let Some(e) = em {
            out.insert("em".into(), e);
        }
        if let Some(sc) = sec {
            out.insert("sec".into(), sc);
        }
        let ir = if !lb { self.iridescence(s) } else { None };
        if let Some(ir) = ir {
            out.insert("ir".into(), ir);
        }
        let dec = if lb { self.lb_decals(s, by_name.get("BaseLayer1").copied()) } else { None };
        if let Some(d) = dec {
            out.insert("dec".into(), d);
        }
        Some(Value::Object(out))
    }

    /// HardSurface %SECONDARY_LAYER: {blend, bc4, tile, bias, base, pfo, sfo, h} and layers[1] set to the Secondary layer, or None
    /// (layers untouched).
    ///
    /// From the HardSurface pixel shader (CONFIRMED arithmetic, the mapping of its inputs to .mtl names INFERRED where marked): the Secondary layer is a height blend against the
    /// Primary one, evaluated before the layers are shaded (evaluated in the zone viewer).  Primary height hP = 1 + HeightBias,
    /// softness sP = max(min(0.25 * falloff, 0.5) * HeightScale, 0.001); Secondary height hS = (r - 0.5) * HeightScale + HeightBias
    /// + 0.5 from its blend map's red, plus wearAmount + SecondaryLayerHeightBias, softness sS = max(min(g * 0.5 + 0.25 * falloff,
    /// min(1 - r, r)) * HeightScale, 0.001) with g the map's green; falloff is the layer file's WearDirtBlendFalloff byte,
    /// HeightBias / HeightScale the layer entry's (0 / 1 in all 19,000 ship entries).  The weights are the overlap areas of [h - s,
    /// h + s]: a_i = w_i * clamp(w_i / (2 s_i)) with w_P = max(hP + sP - m, 0), w_S = max(hS' + sS - m, 0), m = max(hP - sP, hS' -
    /// sS); Primary share = a_P / (a_P + a_S), Secondary share = a_S / (a_P + a_S).  The Secondary maps are sampled at uv * the
    /// layer's tiling (the same as its blend map, so the existing layer tiling, INFERRED to equal LayerTiling * UVTiling).  Needs
    /// the Secondary blend map (TexSlot8 of the layer file, BC1 / BC4 / BC5S in the ship files: a BC4 map has no green, which the
    /// engine reads as 0; the server shows BC4 as grey, so the viewer is told).
    /// WearMasksSecondaryLayer (`wm`; 19 of 330 ship materials; CONFIRMED in HardSurfacePS and the engine's layer-weight
    /// function): 1 gives shift = (wear + SecondaryLayerHeightBias)
    /// * primaryWearFraction, 0 gives shift = wear + SecondaryLayerHeightBias, the shift being what is added to hS above; weights
    /// primaryIntact = (1 - wf) * share, primaryWorn = wf * share, secondary = ws / (wp + ws).  A missing blend map
    /// (`blend` -1, 36 of 330 ship Secondary layers) is sampled as black, the INFERRED default texture.  Not drawn: POM materials
    /// (the Secondary diffuse and the POM height share a texture unit; 4 of 330).
    fn hs_secondary(&mut self, s: &Sub, by_name: &HashMap<&str, &Layer>, layers: &mut Vec<Value>) -> Option<Value> {
        if !gen_has(&s.gen_mask, "SECONDARY_LAYER") || !self.lazy_textures {
            return None;
        }
        let (sl, prim) = (by_name.get("Secondary").copied(), by_name.get("Primary").copied());
        let (sl, prim) = (sl?, prim?);
        if sl.path.is_empty() || prim.path.is_empty() || layers[0].is_null() {
            return None;
        }
        let sf = self.layer_file(&sl.path, &sl.submtl);
        let pf = self.layer_file(&prim.path, &prim.submtl);
        if !secondary_drawn(sf.as_ref()) || pf.is_none() {
            return None;
        }
        let (sf, pf) = (sf.unwrap(), pf.unwrap());
        let bm = sf.texture("TexSlot8");
        let blend = match bm {
            Some(b) => self.texture(b, ""), // -1: no blend map, the viewer samples black (INFERRED default texture)
            None => -1,
        };
        let mut fmt = None;
        if let Some(b) = bm {
            if let Some(ent) = self.p4k.find_path(&format!("{}.dds", texture_base(b)), ".dds") {
                fmt = DdsTexture::new(self.p4k, ent, false).ok().map(|t| t.format.name);
            }
        }
        let sec_layer = self.layer_entry(s, Some(sl))?;
        while layers.len() < 2 {
            layers.push(Value::Null);
        }
        let tile = sec_layer["tiling"].clone();
        layers[1] = sec_layer;
        let p = &s.params;
        Some(json!({"blend": blend, "bc4": matches!(fmt, Some("bc4") | Some("bc4s")), "tile": tile,
                    "wm": if num_param(p, "WearMasksSecondaryLayer", 0.0) != 0.0 { 1 } else { 0 },
                    "bias": num_param(p, "SecondaryLayerHeightBias", 0.0), "base": num_param(p, "WearBlendBase", 0.0),
                    "pfo": cl(num_param(&pf.params, "WearDirtBlendFalloff", 1.0), 0.0, 1.0),
                    "sfo": cl(num_param(&sf.params, "WearDirtBlendFalloff", 1.0), 0.0, 1.0),
                    "h": [prim.height_scale, prim.height_bias, sl.height_scale, sl.height_bias]}))
    }

    /// LayerBlend_V2 %DECALS (9,463 sub-materials: armor, clothing, FPS weapons, buildings; no ship .mtl has it).
    /// From LayerBlendPS (CONFIRMED unless marked): the decal UV DECAL_TC is baked into the vertex colour; the pixel shader picks one
    /// of three cells (stencil sticker, parallax decal, diffuse decal; evaluated in the viewer).  Slots: TexSlot9 DiffuseDecalMap (`diff`),
    /// parallax decal TexSlot1 diffuse (`pd`), TexSlot2 ddna (`pn`, packed normal + gloss), TexSlot4 specular and TexSlot8
    /// displacement (`ps`: the specular map with the height's red channel in its alpha when both exist; `hs` / `ph` say which halves
    /// are real), stencil TexSlot6 StencilMap / TexSlot7 StencilBreakup (`st`).  The diffuse decal's specular / gloss are
    /// PerMatData.decalParams bytes (b3, b2, b1 = specular rgb, b0 = gloss, / 255): EMPIRICAL mapping to the PublicParams
    /// DiffuseDecalSpec (rgb, 0..1) and DiffuseDecalGloss (0..255; 8,524 of 8,525 sampled armor %DECALS materials carry both).
    /// PomDisplacement / PomHeightBias drive the height march.  Stencil (60 of 8,525 sampled): StencilTiling / StencilBreakupTiling,
    /// StencilOpacity and StencilDiffuseBreakup are the packed-stencil bytes (byte to name mapping INFERRED from the order and the
    /// values), colours as in `stencil`.  The coverage of the decal comes from BaseLayer1's wear definition (`sm` ShadingModel and
    /// `fo` WearDirtBlendFalloff of the layer file, 1 without wear).  None without %DECALS or without any
    /// decal map.
    fn lb_decals(&mut self, s: &Sub, base1: Option<&Layer>) -> Option<Value> {
        if !gen_has(&s.gen_mask, "DECALS") {
            return None;
        }
        let p = &s.params;
        let t = |this: &mut Self, slot: &str, chan: &str| -> i64 {
            match s.texture(slot) {
                Some(f) => this.texture(f, chan),
                None => -1,
            }
        };
        let diff = t(self, "TexSlot9", "");
        let pd = t(self, "TexSlot1", "");
        let mut out = Map::new();
        out.insert("diff".into(), json!(diff));
        out.insert("pd".into(), json!(pd));
        out.insert("pn".into(), json!(-1));
        out.insert("ps".into(), json!(-1));
        out.insert("hs".into(), json!(false));
        out.insert("ph".into(), json!(false));
        out.insert("spec".into(), g4(&vec3(p, "DiffuseDecalSpec").unwrap_or([0.04; 3])));
        out.insert("gloss".into(), json!(rnd(cl(num_param(p, "DiffuseDecalGloss", 128.0), 0.0, 255.0) / 255.0, 4)));
        out.insert("disp".into(), json!(num_param(p, "PomDisplacement", 0.0)));
        out.insert("bias".into(), json!(num_param(p, "PomHeightBias", 1.0)));
        if let Some(n) = s.texture("TexSlot2").filter(|n| pd >= 0 && is_ddna(n)) {
            let v = self.texture(n, "ng");
            out.insert("pn".into(), json!(v));
        }
        let (sp, h) = (s.texture("TexSlot4"), s.texture("TexSlot8"));
        if let (true, Some(h)) = (pd >= 0, h) {
            // specular rgb + height (red) in the alpha, one PNG (texserve build_hw)
            let v = self.texture(sp.unwrap_or(""), &format!("hw:{}", h));
            out.insert("ps".into(), json!(v));
            out.insert("hs".into(), json!(sp.is_some()));
            out.insert("ph".into(), json!(true));
        } else if let (true, Some(sp)) = (pd >= 0, sp) {
            let v = self.texture(sp, "");
            out.insert("ps".into(), json!(v));
            out.insert("hs".into(), json!(true));
        }
        if let Some(src) = s.texture("TexSlot6") {
            let pal = src.to_lowercase().starts_with("$tintpalettedecal");
            let tid = if pal { -1 } else { self.texture(src, "") };
            if pal || tid >= 0 {
                let col = |k: &str| g4(&vec3(p, k).unwrap_or([1.0; 3]));
                let br = t(self, "TexSlot7", "");
                out.insert(
                    "st".into(),
                    json!({"pal": pal, "tex": tid, "br": br, "tile": num_param(p, "StencilTiling", 1.0), "btile": num_param(p, "StencilBreakupTiling", 1.0),
                           "op": num_param(p, "StencilOpacity", 1.0), "brk": num_param(p, "StencilDiffuseBreakup", 0.0),
                           "tint": num_param(p, "StencilTintOverride", 0.0) as i64,
                           "col": [col("StencilDiffuseColor"), col("StencilDiffuseColor2"), col("StencilDiffuseColor3")]}),
                );
            }
        }
        if out["diff"].as_i64() < Some(0) && out["pd"].as_i64() < Some(0) && !out.contains_key("st") {
            return None;
        }
        if let Some(b) = base1.filter(|b| !b.path.is_empty()) {
            if let Some(f) = self.layer_file(&b.path, &b.submtl) {
                if let Ok(fo) = por(&f.params, "WearDirtBlendFalloff", 1.0) {
                    out.insert("fo".into(), json!(cl(fo, 0.0, 1.0)));
                    if let Ok(sm) = por(&f.params, "ShadingModel", 0.0) {
                        out.insert("sm".into(), json!(sm as i64));
                    }
                }
            }
        }
        Some(Value::Object(out))
    }

    /// `{"materials": [...], "textures": [...]}` of the scene JSON.
    pub fn export(&self) -> (Value, Value) {
        (Value::Array(self.materials.clone()), Value::Array(self.textures.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_rounding() {
        assert_eq!(rnd(0.5f64.powf(1.0 / 2.2), 4), 0.7297);
        assert_eq!(rnd(0.03125, 4), 0.0312); // exact tie: half to even, like Python's round()
        assert_eq!(rnd(0.15625, 4), 0.1562);
        assert_eq!(rnd(-0.00001, 4), -0.0);
        assert_eq!(rnd(2.5, 0), 2.0);
        assert_eq!(rnd(1.0, 3), 1.0);
    }

    #[test]
    fn slot_name_tests() {
        assert!(is_ddna("a/b_ddna.tif") && is_ddna("A_DDN.TIF") && !is_ddna("a_diff.tif"));
        assert_eq!(suffix("Data\\x\\foo_Spec.tif"), "spec");
        assert_eq!(suffix("$TintPaletteDecal"), "");
        assert_eq!(name_tail("a_b.c.d"), "b");
    }

    #[test]
    fn variants_and_modes() {
        assert_eq!(decal_variant("MeshDecal", "%VERTCOLORS%NORMAL_MAP"), "MeshDecal | VERTCOLORS NORMAL_MAP");
        assert_eq!(decal_variant("Illum", ""), "Illum | -");
        assert_eq!(vcol_mode("MeshDecal", "%VERTDATA"), VCOL_MDATA);
        assert_eq!(vcol_mode("Illum", "%VERTCOLORS%USE_DAMAGE_MAP"), VCOL_DAMAGE);
        assert_eq!(vcol_mode("HardSurface", "%VERTCOLORS"), VCOL_ALPHA);
        assert!(is_translucent("GlassPBR", 1.0, 1.0) && is_translucent("Illum", 0.5, 0.0) && !is_translucent("Illum", 0.5, 0.4));
    }

    #[test]
    fn hair_albedo_default_is_dark_brown() {
        let h = Hair { mel: [0.5, 0.0, 0.0], tint: [1.0; 3], mul: [1.0; 3], dye: [0.0; 4], dcol: [1.0; 3], cards: false };
        let c = hairpbr_albedo(&h, 0.5, 1.0);
        assert!(c[0] > c[1] && c[1] > c[2] && c[0] < 0.99);
    }

    #[test]
    fn glow_conversion() {
        let mut s = mtl_stub();
        s.glow = 1.0;
        assert_eq!(glow_intensity(&s), 400.0);
        s.glow_amount = Some(1.0);
        assert!((glow_intensity(&s) - 4.0).abs() < 1e-12);
    }

    fn mtl_stub() -> SubMaterial {
        SubMaterial {
            name: String::new(),
            shader: "Illum".into(),
            surface: String::new(),
            gen_mask: String::new(),
            textures: vec![],
            tiles: vec![],
            texmods: vec![],
            layers: vec![],
            params: vec![],
            diffuse: [1.0; 3],
            opacity: 1.0,
            emissive: [0.0; 3],
            specular: [0.04; 3],
            shininess: 0.0,
            alpha_test: 0.0,
            glow: 0.0,
            glow_amount: None,
            diffuse_tex: None,
        }
    }
}
