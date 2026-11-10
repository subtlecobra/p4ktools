//! Planet surface colour and ecosystem maps, baked from the body's `.ale` definition.
//!
//! What the data is (EMPIRICAL on Daymar `stanton2b`, live archive 2026-11):
//! * `data.splatMap` is a `w * h * 4` byte array, `w x h = data.globalSplatXY` (512 x 512 on Daymar): per texel `[R, G, B, 0]`.
//!   `R` equals the climate map's G channel (the temperature axis: it correlates -0.96 with |latitude|), `G` the climate map's R channel,
//!   and `B` is `class << 4` with `class` 0..15; `B` is byte for byte the `*_splat.dds` R8 map (same 16 values and counts).
//! * `data.nearColorGlossLUT.mip0` / `data.bedrockColorGlossLUT.mip0` are 128 x 128 RGBA LUTs (row-major, `[row][col]`, A = gloss),
//!   followed by mips 64 .. 2.  Indexing them with `row = G >> 1`, `col = R >> 1` gives physically sensible colour (bright ice towards
//!   the poles, mean luminance 154 at |lat| > 75 against 125 at the equator for the surface LUT; the transposed order gives 107 / 134).
//!   The row/column order is therefore INFERRED from that physical check, not read from the engine; `>> 1` is INFERRED too (256 -> 128).
//! * The class nibble selects one of `General.uniqueEcoSystemsGUIDs` (16 entries on Daymar, 16 classes).  CONFIRMED only as far as
//!   the compute shader `PlanetTerrainVT@PlanetTerrainVT_UberBake` (GLCompute `PlanetTerrainVT_UberBake`) goes: it reads the splat
//!   texture with `floor(value * 15.9375)` (= the high nibble of the byte) and uses the result as the array layer of `AlbedoEcoSystem`
//!   / `PlanetElevation` (Texture2DArray).  That the layer order equals the order of `uniqueEcoSystemsGUIDs` is INFERRED (counts match).
//! * `Data/Libs/EcoSystems/PU/**/*.eco` are plain JSON `{GUID, name, SoundEffect, Textures{ecoSystemClimateTexture, ...}}` (131 files, all
//!   under `system/planet_techv4/`).
//!
//! Not known: how the engine mixes the surface and bedrock LUTs (soil amount, slope), and what `groundTexIDLUT` / `brushIDLUT` feed
//! (they are 128 x 128 maps over the same coordinates; `groundTexIDLUT` holds values 66..111, not indices into the 32 ground layers).

use std::collections::HashMap;

use serde_json::{json, Value};

/// A 128 x 128 colour LUT (RGBA, row-major).
pub const LUT_SIDE: usize = 128;

/// The splat map of a definition: `(width, height, bytes)` with `[R, G, B, A]` per texel, or `None` when the body has none.
pub fn splat_rgba(def: &Value) -> Option<(usize, usize, Vec<u8>)> {
    let d = &def["data"];
    let xy = d["globalSplatXY"].as_array()?;
    let (w, h) = (xy.first()?.as_u64()? as usize, xy.get(1)?.as_u64()? as usize);
    let arr = d["splatMap"].as_array()?;
    if w == 0 || h == 0 || arr.len() != w * h * 4 {
        return None;
    }
    Some((w, h, arr.iter().map(|v| v.as_u64().unwrap_or(0).min(255) as u8).collect()))
}

/// Mip 0 of `nearColorGlossLUT` (`which = "nearColorGlossLUT"`) or `bedrockColorGlossLUT`, as 128 x 128 RGBA.
pub fn colour_lut(def: &Value, which: &str) -> Option<Vec<[u8; 4]>> {
    let rows = def["data"][which]["mip0"].as_array()?;
    if rows.len() != LUT_SIDE {
        return None;
    }
    let mut out = Vec::with_capacity(LUT_SIDE * LUT_SIDE);
    for r in rows {
        let r = r.as_array()?;
        if r.len() != LUT_SIDE {
            return None;
        }
        for c in r {
            let c = c.as_array()?;
            let b = |i: usize| c.get(i).and_then(Value::as_u64).unwrap_or(0).min(255) as u8;
            out.push([b(0), b(1), b(2), b(3)]);
        }
    }
    Some(out)
}

/// Opaque RGBA of the surface colour: every splat texel looks its `(R, G)` up in the LUT (see the module doc for the order).
pub fn lut_colour_rgba(splat: &[u8], lut: &[[u8; 4]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(splat.len());
    for t in splat.chunks_exact(4) {
        let c = lut[(t[1] as usize >> 1) * LUT_SIDE + (t[0] as usize >> 1)];
        out.extend([c[0], c[1], c[2], 255]);
    }
    out
}

/// The ecosystem class of each texel (high nibble of the splat B channel).
pub fn classes(splat: &[u8]) -> Vec<u8> {
    splat.chunks_exact(4).map(|t| t[2] >> 4).collect()
}

/// Majority class in the `(2r+1)^2` window around every texel (wraps in longitude, clamps at the poles; ties go to the lower class).  A VIEWER
/// HEURISTIC: the raw classes are salt-and-pepper dithered (the shader picks among the 4 gathered texels with `PlanetRandomOffset`), so a
/// regional picture needs a filter; the filter is not the engine's.
pub fn majority_classes(classes: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
    let mut out = vec![0u8; classes.len()];
    let r = r as isize;
    for y in 0..h as isize {
        for x in 0..w as isize {
            let mut n = [0u32; 16];
            for dy in -r..=r {
                let yy = (y + dy).clamp(0, h as isize - 1) as usize;
                for dx in -r..=r {
                    let xx = (x + dx).rem_euclid(w as isize) as usize;
                    n[(classes[yy * w + xx] & 15) as usize] += 1;
                }
            }
            let best = (0..16).fold(0, |b, i| if n[i] > n[b] { i } else { b });
            out[y as usize * w + x as usize] = best as u8;
        }
    }
    out
}

/// A categorical colour for class `i` (golden-angle hue steps, fixed saturation): only to tell classes apart, it carries no meaning.
pub fn class_colour(i: usize) -> [u8; 3] {
    let h = (i as f64 * 137.508) % 360.0;
    let (s, v) = (0.62, 0.92);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [((r + m) * 255.0).round() as u8, ((g + m) * 255.0).round() as u8, ((b + m) * 255.0).round() as u8]
}

/// Opaque RGBA of the ecosystem class map (palette from [`class_colour`]).
pub fn class_rgba(classes: &[u8]) -> Vec<u8> {
    let pal: Vec<[u8; 3]> = (0..16).map(class_colour).collect();
    let mut out = Vec::with_capacity(classes.len() * 4);
    for &c in classes {
        let p = pal[(c & 15) as usize];
        out.extend([p[0], p[1], p[2], 255]);
    }
    out
}

/// One `.eco` file.
#[derive(Debug, Clone, PartialEq)]
pub struct Eco {
    pub guid: String,
    pub name: String,
    pub textures: Vec<(String, String)>,
}

/// Parse one `.eco` (plain JSON, EMPIRICAL: `GUID`, `name`, `Textures{...}`).
pub fn parse_eco(d: &[u8]) -> Option<Eco> {
    let v: Value = serde_json::from_slice(d).ok()?;
    let textures = v["Textures"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()).unwrap_or_default();
    Some(Eco { guid: v["GUID"].as_str()?.to_lowercase(), name: v["name"].as_str().unwrap_or("").to_string(), textures })
}

/// `crater_hill_01/crater_hill_01` from an `.eco`'s `elevationTexture`.  The tile lives in `Data/Textures/planets/terrain/`, with its height in
/// `<stem>_elev.dds` (the engine rewrites `_displ.r16` to `_elev.dds`, CONFIRMED).
pub fn tile_stem(e: &Eco) -> Option<String> {
    let f = &e.textures.iter().find(|(k, _)| k == "elevationTexture")?.1;
    let f = f.replace('\\', "/");
    Some(f.strip_suffix("_displ.r16").or_else(|| f.strip_suffix("_displ.dds")).unwrap_or(f.rsplit_once('.').map_or(&f, |x| x.0)).to_string())
}

/// Mean height of a tile in metres, from its little endian R16 values: `(v / 65535 - 0.5) * 2000`.  CONFIRMED scale: the engine's
/// constant is 2000.  EMPIRICAL: on canyon_coast_01, one of only 2 tiles with a `_data.json`, this reproduces that file's avg, min and max to
/// three decimals, which is why the means are computed here for every tile.
pub fn tile_mean(r16: &[u8]) -> Option<f64> {
    let n = r16.len() / 2;
    if n == 0 {
        return None;
    }
    let sum: u64 = r16.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u64).sum();
    Some((sum as f64 / n as f64 / 65535.0 - 0.5) * 2000.0)
}

/// One of the 16 ecosystem classes of a body.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassSlot<'a> {
    pub guid: Option<String>,
    pub eco: Option<&'a Eco>,
    /// past the resolved GUIDs: the engine fills it with ecosystem 0 of its own list, which is not known here
    pub padded: bool,
}

/// The engine's class table: `(slots, GUIDs it drops)`.  CONFIRMED against the engine: the
/// GUIDs of `uniqueEcoSystemsGUIDs` are taken in order and only those that resolve to an ecosystem are appended, so a GUID with no
/// `.eco` drops out and every later class moves down by one; `RegenerateContent` then pads to 16 with 0.
pub fn class_slots<'a>(def: &Value, ecos: &'a HashMap<String, Eco>) -> (Vec<ClassSlot<'a>>, Vec<String>) {
    let guids = def["data"]["General"]["uniqueEcoSystemsGUIDs"].as_array().cloned().unwrap_or_default();
    let (mut slots, mut dropped) = (vec![], vec![]);
    for g in guids.iter().filter_map(Value::as_str).map(str::to_lowercase) {
        match ecos.get(&g) {
            Some(e) if slots.len() < 16 => slots.push(ClassSlot { guid: Some(g), eco: Some(e), padded: false }),
            _ => dropped.push(g),
        }
    }
    while slots.len() < 16 {
        slots.push(ClassSlot { guid: None, eco: None, padded: true });
    }
    (slots, dropped)
}

/// The mean height of each class's tile: `means[class]`, None for a padded class or one without tile data.  `read` gives the mean of a stem.
pub fn class_means(def: &Value, ecos: &HashMap<String, Eco>, read: impl Fn(&str) -> Option<f64>) -> Vec<Option<f64>> {
    class_slots(def, ecos).0.iter().map(|s| s.eco.and_then(tile_stem).and_then(|t| read(&t))).collect()
}

/// The part of the engine's per-class detail term that a whole-planet view can show, in metres per splat texel.
///
/// CONFIRMED against the engine: height = global map + `fGScale` * tile term, the tile term being `(tile / 65535 - 0.5) * 2000`, blended over the four
/// surrounding splat texels.  But a tile repeats every `fFractalScale` texels (2.3 on Daymar, about 4 km), about one vertex of the globe, so sampling
/// its shape would only alias.  What survives at this scale is each class's mean, so that is drawn.  The 3 x 3 box is a viewer filter: the classes are
/// dithered at texel scale, and the average over neighbours estimates their mix.  Unknown classes count as 0.
pub fn detail_metres(classes: &[u8], w: usize, h: usize, means: &[Option<f64>], g_scale: f64) -> Vec<f32> {
    let m = |x: usize, y: usize| means.get((classes[y * w + x] & 15) as usize).copied().flatten().unwrap_or(0.0);
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            let mut n = 0.0;
            for dy in -1i64..=1 {
                let yy = y as i64 + dy;
                if yy < 0 || yy >= h as i64 {
                    continue;
                }
                for dx in -1i64..=1 {
                    sum += m((x as i64 + dx).rem_euclid(w as i64) as usize, yy as usize);
                    n += 1.0;
                }
            }
            out.push((sum / n * g_scale) as f32);
        }
    }
    out
}

/// The ecosystem table of a body: one entry per class index with the file the GUID names (or `null` when no `.eco` has that GUID) and the share
/// of texels of that class.  `ecos` maps lower case GUID -> [`Eco`].
pub fn ecosystem_legend(def: &Value, classes: &[u8], ecos: &HashMap<String, Eco>) -> Value {
    let (slots, dropped) = class_slots(def, ecos);
    let resolved = slots.iter().filter(|s| !s.padded).count();
    let mut count = [0usize; 16];
    for &c in classes {
        count[(c & 15) as usize] += 1;
    }
    let total = classes.len().max(1) as f64;
    let rows: Vec<Value> = (0..16)
        .filter(|&i| count[i] > 0 || i < resolved)
        .map(|i| {
            let s = &slots[i];
            let eco = s.eco;
            let c = class_colour(i);
            json!({"index": i, "guid": s.guid, "padded": s.padded, "name": eco.map(|e| e.name.clone()), "share": count[i] as f64 / total,
                   "colour": format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]),
                   "textures": eco.map(|e| e.textures.iter().map(|(k, v)| json!({"kind": k, "file": v})).collect::<Vec<_>>())})
        })
        .collect();
    json!({"classes": rows, "unmatchedClasses": (resolved..16).filter(|&i| count[i] > 0).collect::<Vec<_>>(), "droppedGuids": dropped})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> Value {
        let mut lut: Vec<Value> = vec![];
        for r in 0..128 {
            lut.push(json!((0..128).map(|c| json!([r, c, 7, 9])).collect::<Vec<_>>()));
        }
        json!({"data": {"globalSplatXY": [2, 1], "splatMap": [20, 40, 0x30, 0, 255, 255, 0xf0, 0],
               "nearColorGlossLUT": {"mip0": lut}, "General": {"uniqueEcoSystemsGUIDs": ["AA-1", "bb-2"]}}})
    }

    #[test]
    fn splat_lut_lookup_uses_g_as_row_and_r_as_column() {
        let d = def();
        let (w, h, s) = splat_rgba(&d).unwrap();
        assert_eq!((w, h), (2, 1));
        let lut = colour_lut(&d, "nearColorGlossLUT").unwrap();
        assert_eq!(lut_colour_rgba(&s, &lut), [20, 10, 7, 255, 127, 127, 7, 255]);
        assert_eq!(classes(&s), [3, 15]);
        assert!(splat_rgba(&json!({"data": {"globalSplatXY": [2, 2], "splatMap": [1]}})).is_none());
    }

    #[test]
    fn class_detail_uses_tile_means() {
        let e = parse_eco(br#"{"GUID":"aa","name":"n","Textures":{"elevationTexture":"crater_hill_01/crater_hill_01_displ.r16"}}"#).unwrap();
        assert_eq!(tile_stem(&e).as_deref(), Some("crater_hill_01/crater_hill_01"));
        // canyon_coast_01's real extremes, from its _data.json
        assert!((tile_mean(&28835u16.to_le_bytes()).unwrap() + 120.012).abs() < 1e-3);
        assert!((tile_mean(&33423u16.to_le_bytes()).unwrap() - 20.005).abs() < 1e-3);
        assert_eq!(tile_mean(b""), None);
        let d = json!({"data": {"General": {"uniqueEcoSystemsGUIDs": ["AA", "bb"]}}});
        let ecos: HashMap<String, Eco> = [(e.guid.clone(), e)].into();
        let m = class_means(&d, &ecos, |s| (s == "crater_hill_01/crater_hill_01").then_some(100.0));
        assert_eq!((m[0], m[1], m.len()), (Some(100.0), None, 16));
        // an unresolved GUID before a resolved one: the resolved one moves up to class 0
        let d2 = json!({"data": {"General": {"uniqueEcoSystemsGUIDs": ["zz", "AA"]}}});
        let (slots, dropped) = class_slots(&d2, &ecos);
        assert_eq!((slots[0].guid.as_deref(), slots[1].padded, dropped), (Some("aa"), true, vec!["zz".to_string()]));
        assert_eq!(class_means(&d2, &ecos, |_| Some(5.0))[0], Some(5.0));
        // one class everywhere: its mean times fGScale, whatever the filter does
        assert!(detail_metres(&[0; 12], 4, 3, &m, 2.0).iter().all(|v| *v == 200.0));
        // the box wraps in longitude and stops at the poles: a class-1 column counts as 0 for its neighbours on both sides
        let c = [1, 0, 0, 0, 1, 0, 0, 0];
        let g = detail_metres(&c, 4, 2, &m, 1.0);
        assert_eq!(g[2], 100.0);
        assert!((g[3] - 200.0 / 3.0).abs() < 1e-3 && (g[1] - 200.0 / 3.0).abs() < 1e-3);
        assert!(g.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn eco_files_and_legend() {
        let e = parse_eco(br#"{"GUID":"AA-1","name":"flat","Textures":{"ecoSystemClimateTexture":"a/b.dds"}}"#).unwrap();
        assert_eq!((e.guid.as_str(), e.name.as_str()), ("aa-1", "flat"));
        let d = def();
        let (_, _, s) = splat_rgba(&d).unwrap();
        let ecos: HashMap<String, Eco> = [(e.guid.clone(), e)].into();
        let l = ecosystem_legend(&d, &classes(&s), &ecos);
        let rows = l["classes"].as_array().unwrap();
        let row = |i: u64| rows.iter().find(|r| r["index"] == i).cloned();
        assert_eq!(row(0).unwrap()["name"], "flat");
        // bb-2 has no .eco: it drops out, so only class 0 is resolved and the used classes 3 and 15 are padding, as in the engine
        assert_eq!(l["droppedGuids"], json!(["bb-2"]));
        assert!(row(1).is_none());
        let c3 = row(3).unwrap();
        assert_eq!((c3["share"].clone(), c3["padded"].clone(), c3["name"].clone()), (json!(0.5), json!(true), Value::Null));
        assert_eq!(l["unmatchedClasses"], json!([3, 15]));
        assert_ne!(class_colour(0), class_colour(1));
        // a lone odd texel is voted away, the wrap joins the two edges
        let mut c = vec![2u8; 25];
        c[12] = 7;
        assert!(majority_classes(&c, 5, 5, 1).iter().all(|&v| v == 2));
        assert_eq!(majority_classes(&[1, 1, 3, 1], 4, 1, 1), [1, 1, 1, 1]);
    }
}
