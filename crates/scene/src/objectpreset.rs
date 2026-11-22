//! Object preset (`.opr`) files: the brush and ground-cover lists a planet's `.ale` `objectPresetLUT` points at.
//!
//! Evidence (EMPIRICAL on the live archive, 2026-11; nothing here is checked against the engine unless a line says CONFIRMED):
//! * 501 files under `Data/Libs/ObjectPresets/`, all plain JSON `{GUID, name, color, colorGround, windOcclusionHeight, bCenterSpawn,
//!   ObjectPresetGroups: [...]}`.  A group has `name`, `category`, `probability` (0..100), an `Advanced Properties` object, a
//!   `BrushProperties` array and a `Procedural Distribution` object with a `GroundCoverProperties` array.
//! * Ground-cover entries name a `.cgf` (`sPath`) with `nInstances`, `fMinScale` / `fMaxScale`, `fZOffset`, `fXYOffset` and
//!   `bAlignToSurface`; brush entries name a `.cgf` with `nNumInstances`, scale/offset ranges, random rotation flags and per-entry
//!   `fMinSpawnMask` / `fMaxSpawnMask` / `fMinSlope` / `fMaxSlope`.
//! * Daymar's `.ale` `objectPresetLUT` is 128 x 128 and selects among `General.uniqueObjectPresetsGUIDs`, but how the LUT value,
//!   the per-ecosystem `ObjectPresetSpawnMask` and these group properties combine is NOT known here; [`ScatterItem`] is the flat
//!   form a viewer's scatter pass can work with, not a statement of the engine's rule.
//!
//! `parse_opr` / `scatter_items` are pure on bytes so they are unit tested without the archive.

use std::collections::HashMap;

use serde_json::Value;

/// One `.opr` file.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPreset {
    pub guid: String,
    pub name: String,
    pub color: [f32; 4],
    pub color_ground: [f32; 4],
    pub wind_occlusion_height: f32,
    pub b_center_spawn: bool,
    pub groups: Vec<ObjectPresetGroup>,
}

/// One `ObjectPresetGroups` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPresetGroup {
    pub name: String,
    pub category: String,
    /// 0..100 as stored (EMPIRICAL: 100 for the dense ground-cover groups, 5 for the sparse "generic" brush group on Daymar).
    pub probability: f32,
    pub min_elevation: f32,
    pub max_elevation: f32,
    pub min_slope: f32,
    pub max_slope: f32,
    pub min_spawn_mask: f32,
    pub max_spawn_mask: f32,
    pub ground: Vec<GroundCover>,
    pub brush: Vec<Brush>,
}

/// One `Procedural Distribution` -> `GroundCoverProperties` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundCover {
    pub path: String,
    pub instances: u32,
    pub lod_ratio: u32,
    pub view_distance_ratio: u32,
    pub min_scale: f32,
    pub max_scale: f32,
    pub material_override: String,
    pub plane_max_dist_error: f32,
    pub z_offset: f32,
    pub xy_offset: f32,
    pub align_to_surface: bool,
    pub align_to_river: bool,
}

/// One `BrushProperties` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Brush {
    pub path: String,
    pub instances: u32,
    pub random_rotation_all_axis: bool,
    pub random_90: bool,
    pub random_180_flip: bool,
    pub align_to_river: bool,
    pub lod_ratio: u32,
    pub min_scale: f32,
    pub max_scale: f32,
    pub rotation_range: f32,
    pub random_offset_min: f32,
    pub random_offset_max: f32,
    pub random_z_offset_min: f32,
    pub random_z_offset_max: f32,
    pub min_spawn_mask: f32,
    pub max_spawn_mask: f32,
    pub min_slope: f32,
    pub max_slope: f32,
    pub material_override: String,
    /// `arrayBrushParticles` + the lone `structBrushParticle`, kept for the particle pass.
    pub particles: Vec<BrushParticle>,
}

/// A brush particle binding (`.opr` `structBrushParticle` / `arrayBrushParticles`), stored for the particle pass.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushParticle {
    pub library: String,
    pub probability: f32,
    pub offset: [f32; 3],
    pub prime: bool,
    pub mip_level: u32,
    pub scale: f32,
    pub rotation: [f32; 3],
    pub name: String,
}

/// One placeable item of a preset: the flat form a scatter pass consumes.
#[derive(Debug, Clone, PartialEq)]
pub struct ScatterItem {
    pub path: String,
    pub instances: u32,
    pub probability: f32,
    pub min_scale: f32,
    pub max_scale: f32,
    /// `fZOffset` for ground cover, the mean of `fRandomZOffsetMin` / `fRandomZOffsetMax` for brush.
    pub z_offset: f32,
    /// `fXYOffset` for ground cover, the mean of `fRandomOffsetMin` / `fRandomOffsetMax` for brush.
    pub xy_offset: f32,
    pub align_to_surface: bool,
    pub random_rotation: bool,
    /// degrees of yaw freedom (0 for ground cover, 360 for a brush with `bRandomRotationOnAllAxis`)
    pub rotation_range: f32,
    pub min_slope: f32,
    pub max_slope: f32,
    pub min_spawn_mask: f32,
    pub max_spawn_mask: f32,
    pub material_override: String,
}

const ZERO_GUID: &str = "00000000-0000-0000-0000-000000000000";

fn path(v: &Value) -> String {
    v.as_str().unwrap_or("").replace('\\', "/").to_string()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn f(v: &Value, d: f32) -> f32 {
    v.as_f64().unwrap_or(d as f64) as f32
}

fn u(v: &Value, d: u32) -> u32 {
    v.as_u64().unwrap_or(d as u64) as u32
}

fn b(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}

fn vec4(v: &Value) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    if let Some(a) = v.as_array() {
        for (i, x) in out.iter_mut().enumerate() {
            *x = f(a.get(i).unwrap_or(&Value::Null), 0.0);
        }
    }
    out
}

fn vec3(v: &Value) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    if let Some(a) = v.as_array() {
        for (i, x) in out.iter_mut().enumerate() {
            *x = f(a.get(i).unwrap_or(&Value::Null), 0.0);
        }
    }
    out
}

fn parse_particle(v: &Value) -> BrushParticle {
    BrushParticle {
        library: s(&v["particleLibrary"]),
        probability: f(&v["nProbability"], 0.0),
        offset: vec3(&v["vOffset"]),
        prime: b(&v["bPrime"]),
        mip_level: u(&v["nMipLevel"], 1),
        scale: f(&v["fScale"], 1.0),
        rotation: vec3(&v["vRot"]),
        name: s(&v["name"]),
    }
}

fn parse_ground(v: &Value) -> GroundCover {
    GroundCover {
        path: path(&v["sPath"]),
        instances: u(&v["nInstances"], 1),
        lod_ratio: u(&v["nLodRatio"], 100),
        view_distance_ratio: u(&v["nViewDistanceRatio"], 100),
        min_scale: f(&v["fMinScale"], 1.0),
        max_scale: f(&v["fMaxScale"], 1.0),
        material_override: s(&v["sMaterialOverride"]),
        plane_max_dist_error: f(&v["fPlaneMaxDistError"], 0.0),
        z_offset: f(&v["fZOffset"], 0.0),
        xy_offset: f(&v["fXYOffset"], 0.0),
        align_to_surface: b(&v["bAlignToSurface"]),
        align_to_river: b(&v["bAlignToRiver"]),
    }
}

fn parse_brush(v: &Value) -> Brush {
    let mut particles: Vec<BrushParticle> = v["arrayBrushParticles"]
        .as_array()
        .map(|a| a.iter().filter(|p| p.is_object()).map(parse_particle).collect())
        .unwrap_or_default();
    if v["structBrushParticle"].is_object() {
        particles.push(parse_particle(&v["structBrushParticle"]));
    }
    let rotation_range = f(&v["fRandomRotationRange"], 0.0);
    let random_90 = b(&v["b90DegreesRandomRotation"]);
    let random_180_flip = b(&v["bRandom180Flip"]);
    let random_rotation_all_axis = b(&v["bRandomRotationOnAllAxis"]);
    Brush {
        path: path(&v["sPath"]),
        instances: u(&v["nNumInstances"], 1),
        random_rotation_all_axis,
        random_90,
        random_180_flip,
        align_to_river: b(&v["bAlignToRiver"]),
        lod_ratio: u(&v["nLodRatio"], 100),
        min_scale: f(&v["fMinScale"], 1.0),
        max_scale: f(&v["fMaxScale"], 1.0),
        rotation_range,
        random_offset_min: f(&v["fRandomOffsetMin"], 0.0),
        random_offset_max: f(&v["fRandomOffsetMax"], 0.0),
        random_z_offset_min: f(&v["fRandomZOffsetMin"], 0.0),
        random_z_offset_max: f(&v["fRandomZOffsetMax"], 0.0),
        min_spawn_mask: f(&v["fMinSpawnMask"], 0.0),
        max_spawn_mask: f(&v["fMaxSpawnMask"], 1.0),
        min_slope: f(&v["fMinSlope"], 0.0),
        max_slope: f(&v["fMaxSlope"], 1.0),
        material_override: s(&v["sMaterialOverride"]),
        particles,
    }
}

fn parse_group(v: &Value) -> Option<ObjectPresetGroup> {
    let adv = &v["Advanced Properties"];
    let dist = &v["Procedural Distribution"];
    let ground = dist["GroundCoverProperties"]
        .as_array()
        .map(|a| a.iter().filter(|g| g.is_object()).map(parse_ground).collect())
        .unwrap_or_default();
    let brush = v["BrushProperties"]
        .as_array()
        .map(|a| a.iter().filter(|b| b.is_object()).map(parse_brush).collect())
        .unwrap_or_default();
    Some(ObjectPresetGroup {
        name: s(&v["name"]),
        category: s(&v["category"]),
        probability: f(&v["probability"], 100.0),
        min_elevation: f(&adv["fMinElevation"], 0.0),
        max_elevation: f(&adv["fMaxElevation"], f32::MAX),
        min_slope: f(&adv["fGroundCoverMinSlope"], 0.0),
        max_slope: f(&adv["fGroundCoverMaxSlope"], 1.0),
        min_spawn_mask: f(&adv["fMinSpawnMask"], 0.0),
        max_spawn_mask: f(&adv["fMaxSpawnMask"], 1.0),
        ground,
        brush,
    })
}

/// Parse one `.opr` (plain JSON, EMPIRICAL layout above).  `None` for non-JSON or a zero GUID.
pub fn parse_opr(d: &[u8]) -> Option<ObjectPreset> {
    let v: Value = serde_json::from_slice(d).ok()?;
    let guid = v["GUID"].as_str()?.to_lowercase();
    if guid == ZERO_GUID {
        return None;
    }
    let groups = v["ObjectPresetGroups"]
        .as_array()
        .map(|a| a.iter().filter(|g| g.is_object()).filter_map(parse_group).collect())
        .unwrap_or_default();
    Some(ObjectPreset {
        guid,
        name: s(&v["name"]),
        color: vec4(&v["color"]),
        color_ground: vec4(&v["colorGround"]),
        wind_occlusion_height: f(&v["windOcclusionHeight"], 0.0),
        b_center_spawn: b(&v["bCenterSpawn"]),
        groups,
    })
}

impl ObjectPreset {
    /// Every placeable entry of the preset, with the group's probability and constraints folded in.
    ///
    /// VIEWER HEURISTIC (the engine's combination rule is not known): a ground-cover entry inherits the group's slope and spawn-mask
    /// ranges, while a brush entry keeps its own when it has them; the yaw range is 0 for ground cover and 360 only for an all-axis
    /// brush.  The scale/offset fields are the data-driven parts a scatter pass should respect.
    pub fn scatter_items(&self) -> Vec<ScatterItem> {
        let mut out = Vec::new();
        for g in &self.groups {
            for i in &g.ground {
                out.push(ScatterItem {
                    path: i.path.clone(),
                    instances: i.instances,
                    probability: g.probability,
                    min_scale: i.min_scale,
                    max_scale: i.max_scale,
                    z_offset: i.z_offset,
                    xy_offset: i.xy_offset,
                    align_to_surface: i.align_to_surface,
                    random_rotation: false,
                    rotation_range: 0.0,
                    min_slope: g.min_slope,
                    max_slope: g.max_slope,
                    min_spawn_mask: g.min_spawn_mask,
                    max_spawn_mask: g.max_spawn_mask,
                    material_override: i.material_override.clone(),
                });
            }
            for i in &g.brush {
                let random = i.random_rotation_all_axis || i.random_90 || i.random_180_flip;
                let range = if i.random_rotation_all_axis {
                    360.0
                } else if i.random_90 {
                    90.0
                } else if i.random_180_flip {
                    180.0
                } else {
                    i.rotation_range
                };
                out.push(ScatterItem {
                    path: i.path.clone(),
                    instances: i.instances,
                    probability: g.probability,
                    min_scale: i.min_scale,
                    max_scale: i.max_scale,
                    z_offset: (i.random_z_offset_min + i.random_z_offset_max) * 0.5,
                    xy_offset: (i.random_offset_min + i.random_offset_max) * 0.5,
                    align_to_surface: true,
                    random_rotation: random,
                    rotation_range: range,
                    min_slope: i.min_slope,
                    max_slope: i.max_slope,
                    min_spawn_mask: i.min_spawn_mask,
                    max_spawn_mask: i.max_spawn_mask,
                    material_override: i.material_override.clone(),
                });
            }
        }
        out
    }
}

/// Parsed presets keyed by lower case GUID, from `(path, bytes)` pairs (the caller decides what to read).
pub fn index_bytes(names: impl Iterator<Item = (String, Vec<u8>)>) -> HashMap<String, ObjectPreset> {
    let mut out: HashMap<String, ObjectPreset> = HashMap::new();
    for (_name, bytes) in names {
        if let Some(p) = parse_opr(&bytes) {
            out.insert(p.guid.clone(), p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static [u8] {
        br#"{
            "GUID": "AA-BB-CC",
            "name": "test_preset",
            "color": [0.1, 0.2, 0.3, 1.0],
            "colorGround": [0.4, 0.5, 0.6, 0.7],
            "windOcclusionHeight": 2.0,
            "bCenterSpawn": true,
            "ObjectPresetGroups": [
                {
                    "name": "gc",
                    "category": "ground_cover",
                    "probability": 50.0,
                    "Advanced Properties": {
                        "fMinElevation": 1.0,
                        "fMaxElevation": 2.0,
                        "fGroundCoverMinSlope": 0.1,
                        "fGroundCoverMaxSlope": 0.2,
                        "fMinSpawnMask": 0.3,
                        "fMaxSpawnMask": 0.4
                    },
                    "BrushProperties": [
                        {
                            "sPath": "b\\b.cgf",
                            "nNumInstances": 2,
                            "bRandomRotationOnAllAxis": true,
                            "fRandomRotationRange": 360.0,
                            "fMinScale": 1.0,
                            "fMaxScale": 3.0,
                            "fRandomOffsetMin": 0.0,
                            "fRandomOffsetMax": 4.0,
                            "fRandomZOffsetMin": 0.5,
                            "fRandomZOffsetMax": 1.5,
                            "fMinSlope": 0.2,
                            "fMaxSlope": 0.8,
                            "arrayBrushParticles": [],
                            "structBrushParticle": {
                                "particleLibrary": "lib",
                                "nProbability": 20.0,
                                "vOffset": [1.0, 2.0, 3.0],
                                "bPrime": true,
                                "nMipLevel": 2,
                                "fScale": 4.0,
                                "vRot": [0.0, 90.0, 0.0],
                                "name": "p"
                            }
                        }
                    ],
                    "Procedural Distribution": {
                        "GroundCoverProperties": [
                            {
                                "sPath": "a/a.cgf",
                                "nInstances": 5,
                                "nLodRatio": 70,
                                "nViewDistanceRatio": 30,
                                "fMinScale": 0.5,
                                "fMaxScale": 1.5,
                                "sMaterialOverride": "m",
                                "fPlaneMaxDistError": 0.2,
                                "fZOffset": 0.9,
                                "fXYOffset": 2.0,
                                "bAlignToSurface": true,
                                "bAlignToRiver": false
                            }
                        ]
                    }
                }
            ]
        }"#
    }

    #[test]
    fn parses_the_preset_and_groups() {
        let p = parse_opr(sample()).unwrap();
        assert_eq!(p.guid, "aa-bb-cc");
        assert_eq!(p.name, "test_preset");
        assert_eq!(p.color, [0.1, 0.2, 0.3, 1.0]);
        assert_eq!(p.color_ground, [0.4, 0.5, 0.6, 0.7]);
        assert_eq!(p.wind_occlusion_height, 2.0);
        assert!(p.b_center_spawn);
        assert_eq!(p.groups.len(), 1);
        let g = &p.groups[0];
        assert_eq!((g.name.as_str(), g.category.as_str(), g.probability), ("gc", "ground_cover", 50.0));
        assert_eq!((g.min_elevation, g.max_elevation), (1.0, 2.0));
        assert_eq!((g.min_slope, g.max_slope), (0.1, 0.2));
        assert_eq!((g.min_spawn_mask, g.max_spawn_mask), (0.3, 0.4));
        assert_eq!(g.ground.len(), 1);
        let i = &g.ground[0];
        assert_eq!(i.path, "a/a.cgf");
        assert_eq!((i.instances, i.lod_ratio, i.view_distance_ratio), (5, 70, 30));
        assert_eq!((i.min_scale, i.max_scale), (0.5, 1.5));
        assert_eq!((i.z_offset, i.xy_offset, i.plane_max_dist_error), (0.9, 2.0, 0.2));
        assert!(i.align_to_surface);
        assert_eq!(g.brush.len(), 1);
        let b = &g.brush[0];
        assert_eq!(b.path, "b/b.cgf");
        assert_eq!((b.instances, b.lod_ratio), (2, 100));
        assert!(b.random_rotation_all_axis);
        assert_eq!((b.min_scale, b.max_scale), (1.0, 3.0));
        assert_eq!((b.random_offset_min, b.random_offset_max), (0.0, 4.0));
        assert_eq!((b.min_slope, b.max_slope), (0.2, 0.8));
        assert_eq!(b.particles.len(), 1);
        assert_eq!((b.particles[0].library.as_str(), b.particles[0].probability), ("lib", 20.0));
        assert_eq!(b.particles[0].offset, [1.0, 2.0, 3.0]);
        assert!(b.particles[0].prime);
        assert_eq!((b.particles[0].mip_level, b.particles[0].scale), (2, 4.0));
        assert_eq!(b.particles[0].rotation, [0.0, 90.0, 0.0]);
    }

    #[test]
    fn rejects_non_json_and_zero_guids() {
        assert!(parse_opr(b"not json").is_none());
        assert!(parse_opr(br#"{"GUID":"00000000-0000-0000-0000-000000000000","name":"n"}"#).is_none());
        let p = parse_opr(br#"{"GUID":"x","name":"n","ObjectPresetGroups":[{}]}"#).unwrap();
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].probability, 100.0);
        assert!(p.groups[0].ground.is_empty() && p.groups[0].brush.is_empty());
    }

    #[test]
    fn flattens_scatter_items_with_their_constraints() {
        let p = parse_opr(sample()).unwrap();
        let items = p.scatter_items();
        assert_eq!(items.len(), 2);
        let (gc, br) = (&items[0], &items[1]);
        assert_eq!((gc.path.as_str(), gc.instances, gc.probability), ("a/a.cgf", 5, 50.0));
        assert_eq!((gc.min_slope, gc.max_slope), (0.1, 0.2));
        assert_eq!((gc.min_spawn_mask, gc.max_spawn_mask), (0.3, 0.4));
        assert!(!gc.random_rotation && gc.align_to_surface);
        assert_eq!(gc.rotation_range, 0.0);
        assert_eq!(br.path, "b/b.cgf");
        assert_eq!((br.xy_offset, br.z_offset), (2.0, 1.0));
        assert!(br.random_rotation && br.align_to_surface);
        assert_eq!((br.rotation_range, br.min_slope, br.max_slope), (360.0, 0.2, 0.8));
    }

    #[test]
    fn indexes_by_lower_case_guid() {
        let idx = index_bytes(
            [
                (
                    "x.opr".to_string(),
                    br#"{"GUID":"AA-BB-CC","name":"a","ObjectPresetGroups":[]}"#.to_vec(),
                ),
                ("y.opr".to_string(), b"nope".to_vec()),
                (
                    "z.opr".to_string(),
                    br#"{"GUID":"00000000-0000-0000-0000-000000000000"}"#.to_vec(),
                ),
            ]
            .into_iter(),
        );
        assert_eq!(idx.len(), 1);
        assert_eq!(idx["aa-bb-cc"].name, "a");
    }
}
