//! Planet bodies: where a body's definition, elevation and colour live, and the readers for them.
//!
//! Evidence (EMPIRICAL, live archive 2026-11, nothing here is checked against the engine unless a line says CONFIRMED):
//! * A body is the container `Data/ObjectContainers/PU/system/<system>/<id>.socpak` (`stanton2b` = Daymar).  Its `<id>.ale` member is the
//!   planet definition: a `CrChF` chunk file whose payload is one JSON document `{version, GUID, data: {General, atmoParams, cloudParams,
//!   volCloudParams, oceanParams, terrainHeightMapParams, areas, colour LUTs, splatMap ...}}` (version 21, 12 MB for Daymar).  `GUID` equals
//!   the `EntityCryGUID` of the body's `pivot.entxml` (ProceduralEntity).
//! * `General.tSphere.fPlanetTerrainRadius` is the radius in metres (295000 for Daymar, equal to `planetRadius` in the container XML).
//! * The global maps live in `Data/Textures/planets/global/<system>/<id>/<id>_global_<kind>.dds`: `elev` is a 512 x 512 R16 UNORM image
//!   (value 32767 = the lowest texel on Daymar; moons name it `<id>_global_elev`, big planets `<id>_elev`, found through the terrain material, see [`resolve_maps`]), `starmap_diff` a colour image, `clim` the climate map, `splat` a R8 index map, `ddn` a normal map.
//!   Equirectangular layout (longitude across, latitude down) is read from the image content (pole streaks), not from the engine.
//! * The metres per elevation unit are NOT known; the page treats `fScaleH` as an INFERRED guess (see the doc).
//!
//! `read_ale` / `ale_summary` / `list_bodies` are pure on bytes / names so they are unit tested without the archive.

use p4k_formats::dds;
use p4k_formats::png::png_rgba;
use p4k_formats::socpak::SocZip;
use p4k::P4k;
use serde_json::{json, Value};

use crate::textures::DdsTexture;

/// One body: a folder `planets/global/<system>/<id>/` in the textures.
#[derive(Debug, Clone, PartialEq)]
pub struct BodyFiles {
    /// lower case system key (`stanton`)
    pub system: String,
    /// lower case body id (`stanton2b`)
    pub id: String,
}

impl BodyFiles {
    pub fn container(&self) -> String {
        format!("Data/ObjectContainers/PU/system/{}/{}.socpak", self.system, self.id)
    }
}

/// Candidate bodies from archive paths: every `.../planets/global/<system>/<id>/<file>` folder (case-insensitive).  Folders that are not
/// bodies (`stanton4rivers`) are told apart by the caller through [`BodyFiles::container`].
pub fn list_bodies<'a>(names: impl Iterator<Item = &'a str>) -> Vec<BodyFiles> {
    let mut out: Vec<BodyFiles> = vec![];
    for n in names {
        let low = n.replace('\\', "/").to_lowercase();
        let Some(rest) = low.split("/planets/global/").nth(1) else { continue };
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 {
            out.push(BodyFiles { system: parts[0].to_string(), id: parts[1].to_string() });
        }
    }
    out.sort_by(|a, b| (&a.system, &a.id).cmp(&(&b.system, &b.id)));
    out.dedup();
    out
}

/// The texture files a body's terrain material names, resolved against the archive (`None` = the material names a file that is not there,
/// or names none).  EMPIRICAL: `General.tSphere.sMaterialTerrainPlanet` is a `PlanetTerrain` material whose TexSlot3 = splat (R8 index map),
/// TexSlot4 = climate map, TexSlot8 = elevation (`<id>_global_elev` on moons, `<id>_elev` on big planets; Pyro's `pyro1_elev` is named but
/// absent, ArcCorp's material points at Daymar's file).  The colour map is the diffuse of the sibling `<id>_starmap.mtl`.
/// The terrain material's archive path, with `.mtl` added when the definition leaves it out (EMPIRICAL: Nyx 1, Pyro 5b and Ellis 3 name
/// their material without the extension; `P4k::find_path` needs the full name).
pub fn terrain_material_path(def: &Value) -> String {
    let p = def["data"]["General"]["tSphere"]["sMaterialTerrainPlanet"].as_str().unwrap_or("").replace('\\', "/");
    if p.is_empty() || p.to_lowercase().ends_with(".mtl") {
        p
    } else {
        format!("{}.mtl", p)
    }
}

/// `(TexSlotN, file)` pairs of the body's terrain material (`General.tSphere.sMaterialTerrainPlanet`), empty when it cannot be read.
pub fn terrain_material_slots(p4k: &P4k, def: &Value) -> Vec<(String, String)> {
    let Some(e) = p4k.find_path(&terrain_material_path(def), ".mtl") else { return vec![] };
    let Ok(d) = p4k.read(e) else { return vec![] };
    p4k_formats::mtl::parse_mtl(&d).map(|v| v.into_iter().flat_map(|m| m.textures).collect()).unwrap_or_default()
}

pub fn resolve_maps(p4k: &P4k, def: &Value, b: &BodyFiles) -> Value {
    let mtl_path = terrain_material_path(def);
    let full = |p: &str| -> Option<String> {
        let stem = p.rsplit_once('.').map_or(p, |x| x.0);
        p4k.find_path(&format!("{}.dds", stem), ".dds").map(|e| e.name.to_string())
    };
    let slots = |mtl: &str| -> Vec<(String, String)> {
        let Some(e) = p4k.find_path(mtl, ".mtl") else { return vec![] };
        let Ok(d) = p4k.read(e) else { return vec![] };
        p4k_formats::mtl::parse_mtl(&d).map(|v| v.into_iter().flat_map(|m| m.textures).collect()).unwrap_or_default()
    };
    let tex = terrain_material_slots(p4k, def);
    let get = |slot: &str| tex.iter().find(|(k, _)| k == slot).and_then(|(_, f)| full(f));
    let star_mtl = mtl_path.strip_suffix(".mtl").map(|s| format!("{}_starmap.mtl", s)).unwrap_or_default();
    let star = slots(&star_mtl).into_iter().find(|(k, _)| k == "TexSlot1").and_then(|(_, f)| full(&f));
    // a file in another body's folder is a borrowed placeholder (ArcCorp and Crusader name Daymar's elevation map): not this body's data
    let own = format!("/global/{}/{}/", b.system, b.id);
    let mut out = serde_json::Map::new();
    let mut borrowed = serde_json::Map::new();
    for (k, v) in [("elev", get("TexSlot8")), ("clim", get("TexSlot4")), ("splat", get("TexSlot3")), ("starmap", star)] {
        match v {
            Some(p) if !p.to_lowercase().contains(&own) => {
                borrowed.insert(k.into(), json!(p));
                out.insert(k.into(), Value::Null);
            }
            v => {
                out.insert(k.into(), json!(v));
            }
        }
    }
    out.insert("borrowed".into(), Value::Object(borrowed));
    Value::Object(out)
}

fn u32_at(d: &[u8], o: usize) -> Option<usize> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?) as usize)
}

/// The JSON payload of a `CrChF` chunk file.  EMPIRICAL layout: `"CrChF"` + 3 bytes, u32 chunk count at 8, u32 table offset at 12 (0x10),
/// then 16-byte entries `{u16 type, u16 version, u32 id, u32 size, u32 offset}`; the planet definition is the chunk whose data starts with `{`.
pub fn read_ale(d: &[u8]) -> Result<Value, String> {
    if d.get(..5) != Some(b"CrChF") {
        return Err("not a CrChF file".into());
    }
    let (n, table) = (u32_at(d, 8).ok_or("short")?, u32_at(d, 12).ok_or("short")?);
    for i in 0..n.min(64) {
        let e = table + i * 16;
        let (size, off) = (u32_at(d, e + 8).ok_or("short table")?, u32_at(d, e + 12).ok_or("short table")?);
        let Some(chunk) = d.get(off..off + size) else { continue };
        if chunk.first() == Some(&b'{') {
            return serde_json::from_slice(chunk).map_err(|e| format!("planet JSON: {}", e));
        }
    }
    Err("no JSON chunk".into())
}

/// The small, page-relevant part of a definition (the big arrays - splat map, LUTs, areas - stay out).
pub fn ale_summary(v: &Value) -> Value {
    let d = &v["data"];
    let g = &d["General"];
    let layers: Vec<Value> = g["textureLayers"]["groundLayers"]
        .as_array()
        .map(|a| a.iter().map(|l| json!({"texture": l["textureName"], "surface": l["surfaceTypeName"], "bedrock": l["bIsBedrock"]})).collect())
        .unwrap_or_default();
    let brushes: Vec<Value> = g["brushes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|b| json!({"name": b["name"], "mode": b["paintMode"], "surfaceA": b["surfaceBrush"]["colorGradient"]["gradientColorA"], "surfaceB": b["surfaceBrush"]["colorGradient"]["gradientColorB"]}))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "version": v["version"], "guid": v["GUID"],
        "sphere": g["tSphere"], "rotationSpeed": g["fRotationSpeed"], "temperatureRange": g["vTemperatureRange"], "rainfallRange": g["vAnnualRainfallRange"],
        "heightMap": d["terrainHeightMapParams"]["General"], "ocean": d["oceanParams"], "atmosphere": d["atmoParams"],
        "cloud": d["cloudParams"]["General"], "volCloud": d["volCloudParams"]["General"],
        "groundLayers": layers, "brushes": brushes, "areas": d["areas"].as_array().map_or(0, |a| a.len()),
        "ecosystems": g["uniqueEcoSystemsGUIDs"].as_array().map_or(0, |a| a.len()),
    })
}

/// Parsed definition of a body, or why there is none.
pub fn read_definition(p4k: &P4k, b: &BodyFiles) -> Result<Value, String> {
    let name = format!("Data/ObjectContainers/PU/system/{}/{}.socpak", b.system, b.id);
    let e = p4k.get(&name).ok_or_else(|| format!("{} is not in the archive", name))?;
    let data = p4k.read(e).map_err(|e| e.to_string())?;
    let z = SocZip::open(&data).map_err(|e| e.to_string())?;
    let member = z.namelist().find(|n| n.to_lowercase().ends_with(".ale")).map(String::from).ok_or("the container has no .ale member")?;
    read_ale(&z.read(&member).map_err(|e| e.to_string())?)
}

/// The elevation map as little-endian u16 bytes: `(width, height, bytes)`.
pub fn elevation(p4k: &P4k, path: &str) -> Result<(usize, usize, Vec<u8>), String> {
    let tex = DdsTexture::new(p4k, p4k.get(path).ok_or("no elevation map")?, false)?;
    if tex.format.name != "r16" {
        return Err(format!("elevation map is {}, not r16", tex.format.name));
    }
    let i = (0..tex.dims.len()).find(|&i| tex.available(i)).ok_or("no mip")?;
    let (w, h) = tex.dims[i];
    let bytes = tex.level(i).ok_or("mip unreadable")?;
    if bytes.len() != w * h * 2 {
        return Err("elevation size mismatch".into());
    }
    Ok((w, h, bytes.to_vec()))
}

/// A map from `resolve_maps` as PNG at the largest level up to `max` pixels.
pub fn map_png(p4k: &P4k, path: &str, max: usize) -> Option<Vec<u8>> {
    let tex = DdsTexture::new(p4k, p4k.get(path)?, false).ok()?;
    if !dds::DECODABLE.contains(&tex.format.name) {
        return None;
    }
    let i = (0..tex.dims.len()).find(|&i| tex.available(i) && tex.dims[i].0.max(tex.dims[i].1) <= max).or_else(|| (0..tex.dims.len()).rev().find(|&i| tex.available(i)))?;
    let (w, h) = tex.dims[i];
    let rgba = dds::decode(tex.format.name, &tex.level(i)?, w, h).ok()?;
    Some(png_rgba(w as u32, h as u32, &rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_material_path_adds_the_missing_extension() {
        let def = |p: &str| json!({"data": {"General": {"tSphere": {"sMaterialTerrainPlanet": p}}}});
        assert_eq!(terrain_material_path(&def("Textures/planets/global/nyx/nyx1/nyx1")), "Textures/planets/global/nyx/nyx1/nyx1.mtl");
        assert_eq!(terrain_material_path(&def("textures\\a\\b.MTL")), "textures/a/b.MTL");
        assert_eq!(terrain_material_path(&json!({})), "");
    }

    #[test]
    fn lists_body_folders() {
        let names = [
            "Data/Textures/planets/global/stanton/stanton2b/stanton2b_global_elev.dds",
            "Data/Textures/planets/global/stanton/stanton2b/stanton2b_global_clim.dds",
            "Data/Textures/planets/global/stanton/stanton4/stanton4_elev.dds",
            "Data/Textures/planets/global/default/default_global_elev.dds",
            "Data/Textures/planets/global_data_offset_512.dds",
        ];
        let v = list_bodies(names.iter().copied());
        assert_eq!(v.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), ["stanton2b", "stanton4"]);
        assert_eq!(v[0].container(), "Data/ObjectContainers/PU/system/stanton/stanton2b.socpak");
    }

    fn chunk_file(json: &str) -> Vec<u8> {
        let mut d = b"CrChF\x07\0\0".to_vec();
        d.extend(1u32.to_le_bytes());
        d.extend(0x10u32.to_le_bytes());
        d.extend([0x11, 0, 0x15, 0]);
        d.extend(2u32.to_le_bytes());
        d.extend((json.len() as u32).to_le_bytes());
        d.extend(0x20u32.to_le_bytes());
        d.extend(json.as_bytes());
        d
    }

    #[test]
    fn reads_the_json_chunk_and_summarises() {
        let j = r#"{"version":21,"GUID":"g","data":{"General":{"tSphere":{"fPlanetTerrainRadius":1000.0},"textureLayers":{"groundLayers":[{"textureName":"a/b.dds","surfaceTypeName":"sand","bIsBedrock":false}]}},"areas":[1,2]}}"#;
        let v = read_ale(&chunk_file(j)).unwrap();
        let s = ale_summary(&v);
        assert_eq!(s["sphere"]["fPlanetTerrainRadius"], 1000.0);
        assert_eq!(s["groundLayers"][0]["surface"], "sand");
        assert_eq!(s["areas"], 2);
        assert!(read_ale(b"nope").is_err());
    }
}
