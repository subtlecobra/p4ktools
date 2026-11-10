//! Atmosphere, ocean and cloud parameters of a planet body, for the globe viewer.
//!
//! This module only gathers the parameters of the `.ale` definition (`atmoParams`, `oceanParams`, `cloudParams`, `volCloudParams`), the ocean
//! material's colours and the archive paths of the cloud maps.  It never invents a value: a missing block is `null`, a texture the archive
//! does not hold is reported in `missing`.

use p4k_formats::mtl;
use p4k::P4k;
use serde_json::{json, Map, Value};

/// A `.tif` / `.dds` name from the definition (`textures/planets/...`) as the archive path of the `.dds`, if it exists.
pub fn dds_path(p4k: &P4k, p: &str) -> Option<String> {
    let p = p.replace('\\', "/");
    if p.trim().is_empty() {
        return None;
    }
    let stem = p.rsplit_once('.').map_or(p.as_str(), |x| x.0);
    p4k.find_path(&format!("{}.dds", stem), ".dds").map(|e| e.name.to_string())
}

/// The parameter blocks of a definition, without any archive lookups (pure, unit tested).
///
/// * `atmosphere`: `atmoParams` as is when `General.AtmoHeight > 0`, else `null`.
/// * `ocean`: `{enabled, frozen, oceanRadius, fogDensity, material}`; `oceanRadius` is read as metres relative to the zero of the height formula
///   (INFERRED, see the doc).
/// * `thin`: `cloudParams` when `General.Enabled`, else `null` (the thin global-map layer, engine pass `PlanetAtmospherics@CloudPassPS`).
/// * `vol`: `volCloudParams` when `General.Enabled`, else `null` (volumetric clouds, `PlanetVolumetricClouds@RaymarchPassCS`).
pub fn sky_params(def: &Value) -> Value {
    let d = &def["data"];
    let atmo = &d["atmoParams"];
    let atmosphere = if atmo["General"]["AtmoHeight"].as_f64().is_some_and(|h| h > 0.0) { atmo.clone() } else { Value::Null };
    let oc = &d["oceanParams"];
    let ocean = if oc["General"].is_object() {
        json!({
            "enabled": oc["General"]["Enabled"].as_bool().unwrap_or(false),
            "frozen": oc["General"]["Frozen"].as_bool().unwrap_or(false),
            "oceanRadius": oc["Geometry"]["OceanRadius"],
            "fogDensity": oc["Effects"]["fogDensity"],
            "material": oc["Geometry"]["MaterialOceanPlanet"],
        })
    } else {
        Value::Null
    };
    let on = |v: &Value| v["General"]["Enabled"].as_bool().unwrap_or(false);
    json!({
        "atmosphere": atmosphere,
        "ocean": ocean,
        "thin": if on(&d["cloudParams"]) { d["cloudParams"].clone() } else { Value::Null },
        "vol": if on(&d["volCloudParams"]) { d["volCloudParams"].clone() } else { Value::Null },
    })
}

/// `sky_params` plus archive lookups: `ocean.colors` (the ocean material's `Diffuse`, `WaterShorelineTintColor`, `WaterSurfaceColorVariation`,
/// linear RGB as stored) and `maps` = `{thin, gpm, types}` archive paths (`null` when absent) with `missing` naming the ones the definition
/// names but the archive does not hold.
pub fn sky_info(p4k: &P4k, def: &Value) -> Value {
    let mut s = sky_params(def);
    let mut missing: Vec<String> = vec![];
    let mut maps = Map::new();
    let mut find = |key: &str, name: &Value| {
        let n = name.as_str().unwrap_or("");
        let r = dds_path(p4k, n);
        if r.is_none() && !n.is_empty() {
            missing.push(format!("{}: {}", key, n));
        }
        maps.insert(key.to_string(), json!(r));
    };
    if s["thin"].is_object() {
        find("thin", &def["data"]["cloudParams"]["Textures"]["globalCloudsMap"]);
    }
    if s["vol"].is_object() {
        let t = &def["data"]["volCloudParams"]["Textures"];
        find("gpm", &t["GlobalPropertyMap"]);
        find("types", &t["CloudTypeMap"]);
    }
    if s["ocean"].is_object() {
        if let Some(path) = s["ocean"]["material"].as_str().filter(|p| !p.is_empty()) {
            // EMPIRICAL: the definition names the material with or without the `.mtl` extension
            let path_ext = if path.to_lowercase().ends_with(".mtl") { path.to_string() } else { format!("{}.mtl", path) };
            let sub = p4k.find_path(&path_ext, ".mtl").and_then(|e| p4k.read(e).ok()).and_then(|b| mtl::parse_mtl(&b).ok()).and_then(|v| v.into_iter().next());
            let rgb = |sm: &mtl::SubMaterial, k: &str| -> Value {
                let v: Vec<f64> = sm.params.iter().find(|(n, _)| n == k).map(|(_, v)| v.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default();
                if v.len() == 3 {
                    json!(v)
                } else {
                    Value::Null
                }
            };
            match sub {
                Some(sm) => {
                    s["ocean"]["colors"] = json!({"diffuse": sm.diffuse, "shoreline": rgb(&sm, "WaterShorelineTintColor"), "variation": rgb(&sm, "WaterSurfaceColorVariation"), "shader": sm.shader});
                }
                None => missing.push(format!("ocean material: {}", path)),
            }
        }
    }
    s["maps"] = Value::Object(maps);
    s["missing"] = json!(missing);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_null_when_disabled_or_absent() {
        let d = json!({"data": {
            "atmoParams": {"General": {"AtmoHeight": 5.0}, "Rayleigh": {"Beta": [1.0, 2.0, 3.0]}},
            "oceanParams": {"General": {"Enabled": false, "Frozen": true}, "Geometry": {"OceanRadius": -366.0, "MaterialOceanPlanet": "a.mtl"}},
            "cloudParams": {"General": {"Enabled": false}},
            "volCloudParams": {"General": {"Enabled": true}, "Textures": {"GlobalPropertyMap": "x.tif"}},
        }});
        let s = sky_params(&d);
        assert_eq!(s["atmosphere"]["Rayleigh"]["Beta"][2], 3.0);
        assert_eq!(s["ocean"]["enabled"], false);
        assert_eq!(s["ocean"]["frozen"], true);
        assert_eq!(s["ocean"]["oceanRadius"], -366.0);
        assert!(s["thin"].is_null());
        assert!(s["vol"].is_object());
        let none = sky_params(&json!({"data": {"atmoParams": {"General": {"AtmoHeight": 0.0}}}}));
        assert!(none["atmosphere"].is_null() && none["ocean"].is_null() && none["thin"].is_null() && none["vol"].is_null());
    }
}
