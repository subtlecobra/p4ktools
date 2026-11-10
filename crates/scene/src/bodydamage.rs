//! Per-body-part damage multipliers of the actor health configs (`BodyHealthConfig.*` in Game2.dcb), for the weapons lab.
//!
//! Each config has `bodyPartConfigs[]`: `bodyParts` (refs to `BodyPart.head|torso|leftArm|rightArm|leftLeg|rightLeg`), `damageMultiplier`,
//! `nakedDamageMultiplier`, `stunMultiplier`, `impactForceMultiplier`, `isHeadShot`, `damageCap`.  EMPIRICAL (read on the live archive).
//! Their engine meaning (CONFIRMED against the engine): per part the hit damage is split by the number of parts hit, scaled by the
//! difficulty modifier, THEN the armor is applied (per damage type resistance of the part's single armor item), THEN the body part's damage
//! step multiplies by `damageMultiplier` and, when the part has no armor item, also by `nakedDamageMultiplier` - both only when the hit's
//! "apply limb multiplier" flag is set (it defaults to true; electron discharge / arc hits clear it) - then the optional cap, the
//! ExternalDamageMultiplier status and finally the health stat is adjusted by the total.  `isHeadShot` (used by the head-hit query)
//! changes no damage figure (CONFIRMED: the damage step does not read it).

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::dynv::{arr, f, g};
use crate::itemmaterials::ItemMaterials;

/// `BodyPart.<name>` record name -> the outfit/lab region name.
pub fn region_of(part: &str) -> Option<&'static str> {
    Some(match part {
        "head" => "Head",
        "torso" => "Torso",
        "leftArm" => "LeftArm",
        "rightArm" => "RightArm",
        "leftLeg" => "LeftLeg",
        "rightLeg" => "RightLeg",
        _ => return None,
    })
}

fn ref_guid(v: &Value) -> Option<&str> {
    let a = v.as_array()?;
    if a.len() == 3 && a[0] == "ref" {
        a[2].as_str()
    } else {
        None
    }
}

/// One config record's data -> `{Region: {dmg, naked, stun, impact, head, cap}}`.  `part_name` maps a `BodyPart` record guid to its short name
/// (`head`, `torso` ...).  A part listed in several entries keeps the last one (as a later definition would shadow in the engine mapping: INFERRED).
pub fn parse_config(cfg: &Value, part_name: &dyn Fn(&str) -> Option<String>) -> Value {
    let mut out = Map::new();
    for pc in arr(g(cfg, "bodyPartConfigs")) {
        for p in arr(g(pc, "bodyParts")) {
            let Some(region) = ref_guid(p).and_then(part_name).as_deref().and_then(region_of) else { continue };
            out.insert(
                region.to_string(),
                json!({
                    "dmg": f(g(pc, "damageMultiplier")), "naked": f(g(pc, "nakedDamageMultiplier")), "stun": f(g(pc, "stunMultiplier")),
                    "impact": f(g(pc, "impactForceMultiplier")), "head": g(pc, "isHeadShot").as_bool().unwrap_or(false), "cap": f(g(pc, "damageCap")),
                }),
            );
        }
    }
    Value::Object(out)
}

/// Every `BodyHealthConfig.*` record: `{configs: {"Player": {Region: {...}}, "AI": ...}}`.  Empty without the DataCore.
pub fn read(items: &ItemMaterials) -> Value {
    let mut configs: BTreeMap<String, Value> = BTreeMap::new();
    let Some(d) = items.datacore() else { return json!({"configs": configs}) };
    let part_name = |guid: &str| -> Option<String> {
        let i = items.guid_record(guid)?;
        d.record_name(i).ok()?.strip_prefix("BodyPart.").map(str::to_string)
    };
    for (name, idx) in d.record_index() {
        let Some(short) = name.strip_prefix("BodyHealthConfig.HealthConfig_") else { continue };
        let Some(&i) = idx.first() else { continue };
        let Ok(rec) = d.record(i) else { continue };
        let Ok(v) = d.record_data(&rec, 4) else { continue };
        let cfg = parse_config(&v.to_json(), &part_name);
        if cfg.as_object().is_some_and(|o| !o.is_empty()) {
            configs.insert(short.to_string(), cfg);
        }
    }
    json!({"configs": configs})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(g: &str) -> Option<String> {
        Some(match g {
            "h" => "head",
            "t" => "torso",
            "la" => "leftArm",
            "x" => "unknownPart",
            _ => return None,
        }
        .to_string())
    }

    #[test]
    fn parses_parts_and_shares_one_entry_between_parts() {
        let cfg = json!({"bodyPartConfigs": [
            {"bodyParts": [["ref", 5, "h"]], "damageMultiplier": 1.5, "nakedDamageMultiplier": 4.0, "stunMultiplier": 2.0, "impactForceMultiplier": 1.0, "isHeadShot": true, "damageCap": -1.0},
            {"bodyParts": [["ref", 1, "t"], ["ref", 2, "la"], ["ref", 3, "x"], ["ref", 3, "zz"]], "damageMultiplier": 0.8, "nakedDamageMultiplier": 1.5, "isHeadShot": false}
        ]});
        let r = parse_config(&cfg, &names);
        assert_eq!(r["Head"]["dmg"], 1.5);
        assert_eq!(r["Head"]["naked"], 4.0);
        assert_eq!(r["Head"]["head"], true);
        assert_eq!(r["Torso"]["dmg"], 0.8);
        assert_eq!(r["LeftArm"]["naked"], 1.5);
        assert_eq!(r.as_object().unwrap().len(), 3);
    }

    #[test]
    fn empty_config_gives_no_regions() {
        assert_eq!(parse_config(&json!({}), &names), json!({}));
    }

    /// Real archive: prints the per-part multipliers of the player and AI configs.  Skips without the archive.
    #[test]
    fn real_archive_body_multipliers() {
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = p4k::P4k::open(std::path::Path::new(&path)) else { return };
        let Ok(items) = ItemMaterials::load(&p4k) else { return };
        if items.datacore().is_none() {
            return;
        }
        let v = read(&items);
        for (k, c) in v["configs"].as_object().unwrap() {
            let row: Vec<String> = ["Head", "Torso", "LeftArm", "RightArm", "LeftLeg", "RightLeg"]
                .iter()
                .map(|r| format!("{r} {:.2}/{:.2}{} cap {}", c[*r]["dmg"].as_f64().unwrap_or(f64::NAN), c[*r]["naked"].as_f64().unwrap_or(f64::NAN), if c[*r]["head"] == true { "H" } else { "" }, c[*r]["cap"]))
                .collect();
            eprintln!("{k}: {}", row.join(", "));
        }
        let p = &v["configs"]["Player"];
        assert_eq!(p["Head"]["dmg"], 1.5);
        assert_eq!(p["Head"]["head"], true);
        assert!((p["LeftLeg"]["dmg"].as_f64().unwrap() - 0.8).abs() < 1e-6);
    }
}
