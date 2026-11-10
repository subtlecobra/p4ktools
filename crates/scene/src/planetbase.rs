//! The base colour the V5 terrain bake starts from: the planet-wide soil and geology diffuse maps, blended by the bedrock gradient.
//!
//! CONFIRMED against the engine (game version 4.10.1):
//! * The engine fills the renderer's shared render resources from the planet's terrain material. `GlobalSoilDiffuse` is the material's
//!   TexSlot13, `GlobalGeoDiffuse` TexSlot14 and `PlanetBedrockGradient` TexSlot11 (slot index = TexSlotN - 1, checked on the splat / climate /
//!   offset / elevation slots).
//! * The renderer copies only non-null entries, and the bake binds a default texture for a null one, loaded from
//!   `Engine/EngineAssets/Textures/Black.dds`.
//! * `PlanetTerrainVT_UberBake` (Vulkan): `base = lerp(soil.rgb, geo.rgb, saturate(gradient.x))`. The gradient is sampled at `u = 0`,
//!   `v = frac(elevation * 0.0005 + noise)`. The bake then jitters the soil hue / lightness (`SoilColourVariance`, medium layer) and blends
//!   towards the vegetation tint by vegetation cover. Snow texels take a constant (0.8, 0.84, 1.0). The viewer draws `base` only: it has neither
//!   the medium-layer data nor the GPU vegetation table.
//!
//! EMPIRICAL (LIVE): only `nyx1.mtl` names TexSlot11 / 13 / 14, and its files are not in the archive; no other planet material names them. So on
//! every shipped body all three maps are the default texture, and `base` is that texture's colour.

use p4k_formats::dds;
use p4k::P4k;
use serde_json::{json, Value};

use crate::textures::DdsTexture;

/// The renderer's fallback texture for an unset slot.
pub const DEFAULT_TEXTURE: &str = "Engine/EngineAssets/Textures/Black.dds";

/// The terrain-material slots of the three maps.
pub const SOIL_SLOT: &str = "TexSlot13";
pub const GEO_SLOT: &str = "TexSlot14";
pub const BEDROCK_GRADIENT_SLOT: &str = "TexSlot11";

/// The bake's `lerp(soil, geo, saturate(gradient))`, channels 0..1.
pub fn base_colour(soil: [f32; 3], geo: [f32; 3], gradient: f32) -> [f32; 3] {
    let t = gradient.clamp(0.0, 1.0);
    [0, 1, 2].map(|i| soil[i] + (geo[i] - soil[i]) * t)
}

/// Texel (0, 0) of the largest stored mip of a texture, RGBA 0..1. The default textures are flat colours.
pub fn first_texel(p4k: &P4k, path: &str) -> Result<[f32; 4], String> {
    let ent = p4k.find_path(path, ".dds").ok_or_else(|| format!("{} is not in the archive", path))?;
    let tex = DdsTexture::new(p4k, ent, false)?;
    let lvl = (0..tex.mips).find(|&i| tex.available(i)).ok_or("no stored mip")?;
    let (w, h) = tex.dims[lvl];
    let px = dds::decode(tex.format.name, &tex.level(lvl).ok_or("mip unreadable")?, w, h).map_err(|e| e.to_string())?;
    let c = px.get(..4).ok_or("empty image")?;
    Ok([0, 1, 2, 3].map(|i| c[i] as f32 / 255.0))
}

/// What the bake binds for each map on this body, and the resulting base colour. `named` is the material's slot value (the file it names,
/// present or not), `bound` the texture the bake gets. `colour` is null when the material supplies a map of its own: the viewer does not
/// sample real maps (none ship).
pub fn engine_base(p4k: &P4k, slots: &[(String, String)]) -> Value {
    let named = |slot: &str| slots.iter().find(|(k, _)| k == slot).map(|(_, f)| f.clone()).filter(|f| !f.is_empty());
    let present = |f: &str| {
        let stem = f.rsplit_once('.').map_or(f, |x| x.0);
        p4k.find_path(&format!("{}.dds", stem), ".dds").map(|e| e.name.to_string())
    };
    let mut maps = serde_json::Map::new();
    let mut own = false;
    for (key, slot) in [("soil", SOIL_SLOT), ("geology", GEO_SLOT), ("bedrockGradient", BEDROCK_GRADIENT_SLOT)] {
        let n = named(slot);
        let file = n.as_deref().and_then(present);
        own |= file.is_some();
        maps.insert(key.into(), json!({"slot": slot, "named": n, "bound": file.clone().unwrap_or_else(|| DEFAULT_TEXTURE.to_string()), "default": file.is_none()}));
    }
    let colour = if own {
        Value::Null
    } else {
        match first_texel(p4k, DEFAULT_TEXTURE) {
            Ok(t) => {
                let rgb = [t[0], t[1], t[2]];
                let c = base_colour(rgb, rgb, t[0]);
                json!(c.map(|v| (v * 255.0).round() as u8))
            }
            Err(e) => return json!({"maps": maps, "colour": null, "error": e}),
        }
    };
    json!({"maps": maps, "colour": colour, "defaultTexture": DEFAULT_TEXTURE})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_blends_soil_to_geology_by_the_clamped_gradient() {
        let (s, g) = ([0.2, 0.4, 0.6], [1.0, 0.0, 0.6]);
        assert_eq!(base_colour(s, g, 0.0), s);
        assert_eq!(base_colour(s, g, 1.0), g);
        assert_eq!(base_colour(s, g, 2.0), g);
        assert_eq!(base_colour(s, g, -1.0), s);
        let m = base_colour(s, g, 0.5);
        assert!((m[0] - 0.6).abs() < 1e-6 && (m[1] - 0.2).abs() < 1e-6 && (m[2] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn default_inputs_give_the_default_colour() {
        let black = [0.0; 3];
        assert_eq!(base_colour(black, black, 0.0), black);
    }
}
