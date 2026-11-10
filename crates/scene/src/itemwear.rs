//! Authored default wear / dirt of an item: the DataCore floor the game renders under every other wear source.
//!
//! CONFIRMED against the engine: the entity's geometry resource writes the render node's visual state; each channel is the fixed
//! override if >= 0, else max(forced, max(DataCore floor, net value)).  The DataCore floor is the `SGeometryDataParams` fields Wear,
//! Dirt, Interference, Damage of the entity record's `SGeometryResourceParams` component (the 'Geometry.Geometry.Wear' properties).
//! The wear *driver* is the degradation component's wear accumulator: EffectCurve(age / lifetime) plus usage, starting at InitialAgeRatio x
//! MaxLifetimeHours and InitialUsageRatio.  The viewer only has the floor: INFERRED that a never-used item shows the floor.  Not
//! evaluated: the accumulator part (EffectCurve is an inline BezierCurve whose point layout is unverified).  EMPIRICAL survey of the live
//! archive (29,693 EntityClassDefinition records): InitialAgeRatio and InitialUsageRatio are 0 on all 2,203 records that have a
//! SWearAccumulatorParams, so the floor is the only authored source; it is 0 on every power plant, cooler, shield, quantum drive, weapon
//! and fuse, and non-zero on cargo boxes (0.5-0.6), airlock doors, derelict thruster / missile-rack variants (0.1) and loot containers.

use std::collections::HashSet;

use p4k_formats::dcb::{Dcb, Value};
use p4k_formats::{Error, Result};

use crate::itemmaterials::ItemMaterials;

pub const RECORD: &str = "EntityClassDefinition.";
pub const ZERO: [f64; 4] = [0.0; 4];

/// Lowercase, forward slashes, no leading '/' and no leading 'data/'.
pub fn norm(path: &str) -> String {
    crate::itemmaterials::key(path)
}

/// Every SGeometryDataParams object (root node first, then SubGeometry in order), depth first in field order.
fn walk<'a>(o: &'a Value, out: &mut Vec<&'a Value>) {
    match o {
        Value::Obj(ob) => {
            if ob.struct_name == "SGeometryDataParams" {
                out.push(o);
            }
            for (_, v) in &ob.fields {
                walk(v, out);
            }
        }
        Value::List(l) => l.iter().for_each(|v| walk(v, out)),
        _ => {}
    }
}

/// (wear, dirt, interference, damage) floors for the geometry `geometry_path` of an entity record decoded by `Dcb::record_data` (deep
/// enough to reach the geometry component).  The node whose Geometry.path equals the path wins (a loadout item draws exactly that
/// model); otherwise the root node (the first SGeometryDataParams).  All 0 when the record has none.
pub fn floors(data: &Value, geometry_path: Option<&str>) -> [f64; 4] {
    let mut nodes = vec![];
    walk(data, &mut nodes);
    let Some(first) = nodes.first() else { return ZERO };
    let want = norm(geometry_path.unwrap_or(""));
    let pick = if want.is_empty() {
        first
    } else {
        nodes.iter().find(|n| n.get("Geometry").and_then(|g| g.get("path")).and_then(Value::as_str).map(norm).as_deref() == Some(want.as_str())).unwrap_or(first)
    };
    let f = |k: &str| pick.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    [f("Wear"), f("Dirt"), f("Interference"), f("Damage")]
}

/// `floors` of the entity class `cls` through an `ItemMaterials` (its shared DataCore); zeros when unreadable.
pub fn class_floors(items: Option<&ItemMaterials>, cls: &str, geometry_path: Option<&str>) -> [f64; 4] {
    let Some(d) = items.and_then(|i| i.datacore()) else { return ZERO };
    if cls.is_empty() {
        return ZERO;
    }
    let Some(ids) = d.record_index().get(&format!("{}{}", RECORD, cls)) else { return ZERO };
    let r = d.record(ids[0]).and_then(|rec| d.record_data(&rec, 9));
    match r {
        Ok(data) => floors(&data, geometry_path),
        Err(_) => ZERO,
    }
}

fn compute_floor_paths(d: &Dcb) -> Result<HashSet<String>> {
    let n = d.struct_names();
    let need = |k: &str| n.get(k).copied().ok_or_else(|| Error::msg(format!("KeyError: {}", k)));
    let data_in_node = d.field_offset(need("SGeometryNodeParams")?, "Geometry")?;
    let data_in_root = d.field_offset(need("SGeometryResourceParams")?, "Geometry")? + data_in_node;
    let sd = need("SGeometryDataParams")?;
    let (g_off, w_off, t_off) = (d.field_offset(sd, "Geometry")?, d.field_offset(sd, "Wear")?, d.field_offset(sd, "Dirt")?);
    let m = d.bytes();
    let f32_at = |o: usize| -> Result<f32> { Ok(f32::from_le_bytes(m.get(o..o + 4).ok_or_else(|| Error::msg("out of range"))?.try_into().unwrap())) };
    let mut out = HashSet::new();
    for (cont, base) in [("SGeometryNodeParams", data_in_node), ("SGeometryResourceParams", data_in_root)] {
        for (_k, off) in d.iter_instance_offsets(cont)? {
            if f32_at(off + base + w_off)? > 0.0 || f32_at(off + base + t_off)? > 0.0 {
                let g = u32::from_le_bytes(m[off + base + g_off..off + base + g_off + 4].try_into().unwrap());
                out.insert(norm(&d.text(g)?));
            }
        }
    }
    Ok(out)
}

/// {normalised geometry path} of every SGeometryDataParams in the DataCore (nodes and roots) that authors a Wear or Dirt floor above 0
/// (cached on the reader).  A cheap prefilter: only models in this set need their class record decoded.
pub fn floor_paths(d: &Dcb) -> &HashSet<String> {
    d.floor_paths.get_or_init(|| compute_floor_paths(d).unwrap_or_default())
}

/// `class_floors` that skips the record decode when no record anywhere authors a floor for the model.
pub fn class_floors_for(items: Option<&ItemMaterials>, cls: &str, geometry_path: &str) -> [f64; 4] {
    let Some(d) = items.and_then(|i| i.datacore()) else { return ZERO };
    if !floor_paths(d).contains(&norm(geometry_path)) {
        return ZERO;
    }
    class_floors(items, cls, Some(geometry_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p4k_formats::dcb::Obj;

    fn node(path: &str, wear: f64) -> Value {
        Value::Obj(Obj {
            struct_name: "SGeometryDataParams".into(),
            fields: vec![
                ("Geometry".into(), Value::Obj(Obj { struct_name: "G".into(), fields: vec![("path".into(), Value::Str(path.into()))] })),
                ("Wear".into(), Value::Float(wear)),
            ],
        })
    }

    #[test]
    fn floors_pick_matching_node_else_root() {
        let root = Value::Obj(Obj {
            struct_name: "SGeometryResourceParams".into(),
            fields: vec![("Geometry".into(), node("Data/a.cgf", 0.1)), ("Sub".into(), Value::List(vec![node("b.cgf", 0.5)]))],
        });
        assert_eq!(floors(&root, Some("data\\B.cgf"))[0], 0.5);
        assert_eq!(floors(&root, Some("zzz.cgf"))[0], 0.1);
        assert_eq!(floors(&root, None)[0], 0.1);
        assert_eq!(floors(&Value::Null, None), ZERO);
    }
}
