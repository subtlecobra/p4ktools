//! Data for the FPS Loadout page's carry rules: the body-attach item ports of worn items, the inventory container of an item, and the
//! catalogue of the carried things the weapons index does not hold (grenades, medpens / oxygen pens, mining gadgets).
//!
//! What the records hold (EMPIRICAL, read through /dcb on the live archive):
//!
//! * Worn armor, undersuits and backpacks carry item ports flagged `body_attach` whose names say what they hold: `wep_stocked_N`
//!   (WeaponPersonal Medium / Large, size 2..5, also Gadget), `wep_sidearm` (WeaponPersonal Small), `magazine_attach_N`, `grenade_attach_N`
//!   (WeaponPersonal Grenade, FPS_Deployable Small), `medPen_attach_N` / `oxyPen_attach_N` (FPS_Consumable), `utility_attach_N`
//!   (WeaponPersonal Gadget / Knife, RemovableChip), `gadget_attach_N` (Type Gadget, size 1..2: the mining gadgets) and `backpack`.
//! * The body record has the hand ports `weapon_attach_hand_left` / `_right` (size 1..5, many types) and two `inventory_pocket` ports
//!   (`inventoryinternalonly`, size 0..5).
//! * Grenades are `WeaponPersonal` / `Grenade` (size 1, 900 uSCU), consumables `FPS_Consumable` (`Medical`, `MedPack`, `OxygenCap`, size 1,
//!   180 uSCU), mining gadgets `Gadget` / `Gadget` (size 2, 10200 uSCU).  Every item has `inventoryOccupancyVolume.microSCU` and
//!   `inventoryOccupancyDimensions` (x, y, z in metres).
//! * Which port holds which thing in the game is decided by the engine's port compatibility check (CONFIRMED): size, required tags,
//!   port tags, accepted types / sub types.

use rayon::prelude::*;
use serde_json::{json, Value};

use crate::dynv::{arr, f, g, s, struct_of, truthy};
use crate::gearstats;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::{comp, ItemStats, RECORD};
use crate::swaps::{self, Port};
use crate::weapons::ordered_names;

/// JSON row of a port: {name, types: [[type, [sub types]]], min, max, flags}.
pub fn port_row(p: &Port) -> Value {
    json!({"name": p.name, "types": p.types.iter().map(|(t, subs)| json!([t, subs.iter().collect::<Vec<_>>()])).collect::<Vec<_>>(),
        "min": p.min, "max": p.max, "flags": p.flags.iter().collect::<Vec<_>>()})
}

/// The carry facts of one worn item: its `body_attach` ports (plus the hand / pocket ports of the body record) and its inventory container.
/// `{cls, ports: [port rows], cap (uSCU, 0 = none), dim: [x, y, z] | null}`.
pub fn worn_facts(stats: &ItemStats, cls: &str) -> Value {
    let ports: Vec<Value> = swaps::item_ports(stats, cls)
        .iter()
        .filter(|(_, p)| p.flags.contains("body_attach") || p.name.to_lowercase().starts_with("weapon_attach_hand") || p.flags.contains("inventoryinternalonly"))
        .filter(|(_, p)| !p.types.is_empty() && !p.types.iter().all(|(t, _)| t.starts_with("Char_")))
        .map(|(_, p)| port_row(p))
        .collect();
    let (cap, dim) = match stats.record(cls, 8) {
        Some(data) => gearstats::capacity(stats, &data),
        None => (0, None),
    };
    json!({"cls": cls, "ports": ports, "cap": cap, "dim": dim.map(|d| json!([d.0, d.1, d.2]))})
}

/// Records that are templates, test fixtures or non-carry variants of the folders scanned (mining base items, hacking chips and keycards are `FPS_Consumable`
/// too but not pens): left out of the catalogue.  EMPIRICAL names.
pub fn is_junk(cls: &str) -> bool {
    let l = cls.to_lowercase();
    ["template", "_test", "test_", "miningbase", "keycard", "hackingchip"].iter().any(|k| l.contains(k))
}

/// The kind of carried thing a record is, from its AttachDef: `grenade`, `consumable` or `mining` (a mining gadget), else None.
pub fn kind_of(ty: &str, sub: &str) -> Option<&'static str> {
    match (ty, sub) {
        ("WeaponPersonal", "Grenade") => Some("grenade"),
        ("FPS_Consumable", _) => Some("consumable"),
        ("Gadget", "Gadget") => Some("mining"),
        _ => None,
    }
}

/// Catalogue rows `{cls, name, manufacturer, kind, type, subtype, size, vol (uSCU), dim [x, y, z]}` of every grenade, consumable and mining gadget.
/// Only the folders that hold them are opened (throwables, consumables, devices).
pub fn catalog(items: &ItemMaterials, stats: &ItemStats) -> Vec<Value> {
    let Some(d) = items.datacore() else { return vec![] };
    let names: Vec<(String, usize)> = ordered_names(items, RECORD)
        .into_iter()
        .filter(|(_, i)| {
            let f = d.record_file(*i).map(|f| f.to_lowercase()).unwrap_or_default();
            f.contains("/scitem/weapons/throwable") || f.contains("/scitem/consumables") || f.contains("/scitem/weapons/devices")
        })
        .collect();
    let mut rows: Vec<Value> = names
        .par_iter()
        .filter_map(|(k, i)| {
            let cls = &k[RECORD.len()..];
            let rec = d.record(*i).ok()?;
            let data = d.record_data(&rec, 4).ok()?.to_json();
            if !arr(g(&data, "Components")).iter().any(|c| truthy(c) && struct_of(c) == "SAttachableComponentParams") {
                return None;
            }
            let att = g(comp(&data, "SAttachableComponentParams"), "AttachDef");
            let sub = if truthy(g(att, "SubType")) { s(g(att, "SubType")) } else { "" };
            if is_junk(cls) {
                return None;
            }
            let kind = kind_of(s(g(att, "Type")), sub)?;
            let vol = f(g(g(att, "inventoryOccupancyVolume"), "microSCU"));
            let dm = g(att, "inventoryOccupancyDimensions");
            if vol <= 0.0 {
                return None;
            }
            let ident = stats.identity(cls);
            let mut row = json!({"cls": cls, "name": ident["name"], "manufacturer": ident["manufacturer"], "kind": kind, "type": g(att, "Type"), "subtype": sub,
                "size": g(att, "Size"), "vol": vol as i64, "dim": [f(g(dm, "x")), f(g(dm, "y")), f(g(dm, "z"))]});
            // hands / grips / mixed from the carry grips (absent when the record has none)
            if let Some(Value::Object(h)) = hands_json(&data) {
                row.as_object_mut().unwrap().extend(h);
            }
            Some(row)
        })
        .collect();
    rows.sort_by(|a, b| s(&a["kind"]).cmp(s(&b["kind"])).then(s(&a["name"]).cmp(s(&b["name"]))).then(s(&a["cls"]).cmp(s(&b["cls"]))));
    rows
}

/// One carry grip of an item: which hand it is for and whether it needs the off hand too.
#[derive(Clone, Debug, PartialEq)]
pub struct Grip {
    /// `left`, `right` or `either` (EHandMode LeftHand / RightHand / Total).
    pub hand: &'static str,
    pub off: bool,
}

/// The Player-usable grips of an item record (`EntityComponentCarryable` -> `gripData.grips[]` = `SCarryableGrip`), or None when the item has no
/// grips (not carryable by hand).
///
/// CONFIRMED: `SCarryableGrip` = {carryStyleID, canBeUsedBy (EGripUser 0 AI, 1 Player, 2 All), handMode (EHandMode 0 LeftHand, 1 RightHand,
/// 2 Total = either), gripAction, handGrip, offHandGrip (weak pointer, null = none), ...}; the engine takes the first grip whose
/// canBeUsedBy fits the Player; the item needs a second hand iff that grip's `offHandGrip` is non-null.
/// EMPIRICAL: the enum names in the records are `AI` / `Player` / `All` and `LeftHand` / `RightHand` / `Total`; a null pointer is JSON null.
/// INFERRED: that this null test means "two-handed" in play terms (the engine only names the mechanism).
pub fn grips_of(data: &Value) -> Option<Vec<Grip>> {
    let c = comp(data, "SEntityComponentCarryableParams");
    let list: Vec<&Value> = arr(g(g(c, "gripData"), "grips")).iter().filter(|x| truthy(x)).collect();
    if list.is_empty() {
        return None;
    }
    let grips: Vec<Grip> = list
        .iter()
        .filter(|x| matches!(s(g(x, "canBeUsedBy")), "Player" | "All" | "1" | "2"))
        .map(|x| Grip {
            hand: match s(g(x, "handMode")) {
                "LeftHand" | "0" => "left",
                "RightHand" | "1" => "right",
                _ => "either",
            },
            off: truthy(g(x, "offHandGrip")),
        })
        .collect();
    Some(grips)
}

/// Hands an item needs from its grips: 2 when the first Player-usable grip has an off-hand grip, else 1 (see [`grips_of`]); None without grips.
pub fn hands_of(grips: &[Grip]) -> Option<u8> {
    grips.first().map(|g| if g.off { 2 } else { 1 })
}

/// `{hands: 1|2, grips: [{hand, off}], mixed: bool}` for the index rows (`mixed`: the grips do not all agree on needing the off hand), or None
/// when the record has no usable grip.
pub fn hands_json(data: &Value) -> Option<Value> {
    let grips = grips_of(data)?;
    let hands = hands_of(&grips)?;
    let mixed = grips.iter().any(|x| x.off != grips[0].off);
    Some(json!({"hands": hands, "grips": grips.iter().map(|x| json!({"hand": x.hand, "off": x.off})).collect::<Vec<_>>(), "mixed": mixed}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of("WeaponPersonal", "Grenade"), Some("grenade"));
        assert_eq!(kind_of("FPS_Consumable", "MedPack"), Some("consumable"));
        assert_eq!(kind_of("FPS_Consumable", "OxygenCap"), Some("consumable"));
        assert_eq!(kind_of("Gadget", "Gadget"), Some("mining"));
        assert_eq!(kind_of("WeaponPersonal", "Gadget"), None);
        assert_eq!(kind_of("WeaponPersonal", "Medium"), None);
    }

    #[test]
    fn junk() {
        assert!(is_junk("FPS_Consumable_TEMPLATE"));
        assert!(is_junk("behr_ltp_kinetic_01_MiningBase"));
        assert!(is_junk("Orbgn_None_Consumable_Keycard_Security"));
        assert!(!is_junk("crlf_consumable_healing_01"));
        assert!(!is_junk("Mining_Gadget_SHIN_Sabir"));
    }

    #[test]
    fn port_row_shape() {
        let p = Port::new("wep_sidearm", "body_attach nophys", 1, 1, &[("WeaponPersonal".into(), vec!["Small".into()])], "", "");
        let r = port_row(&p);
        assert_eq!(r["name"], "wep_sidearm");
        assert_eq!(r["types"][0][0], "WeaponPersonal");
        assert_eq!(r["types"][0][1][0], "small");
        assert_eq!(r["min"], 1);
        assert!(r["flags"].as_array().unwrap().iter().any(|x| x == "body_attach"));
    }

    fn synth(grips: Value) -> Value {
        json!({"Components": [{"__struct__": "SEntityComponentCarryableParams", "gripData": {"grips": grips}}]})
    }

    #[test]
    fn grips_two_handed_when_first_player_grip_has_off_hand() {
        let rifle = synth(json!([
            {"canBeUsedBy": "All", "handMode": "RightHand", "offHandGrip": {"gripID": "BarrelGrip"}},
            {"canBeUsedBy": "All", "handMode": "LeftHand", "offHandGrip": null}
        ]));
        let h = hands_json(&rifle).unwrap();
        assert_eq!(h["hands"], 2);
        assert_eq!(h["mixed"], true);
        assert_eq!(h["grips"][0], json!({"hand": "right", "off": true}));
        assert_eq!(h["grips"][1], json!({"hand": "left", "off": false}));
    }

    #[test]
    fn grips_one_handed_and_ai_only_grips_skipped() {
        let pistol = synth(json!([
            {"canBeUsedBy": "AI", "handMode": "Total", "offHandGrip": {"gripID": "x"}},
            {"canBeUsedBy": "Player", "handMode": "Total", "offHandGrip": null}
        ]));
        let h = hands_json(&pistol).unwrap();
        assert_eq!(h["hands"], 1);
        assert_eq!(h["grips"].as_array().unwrap().len(), 1);
        assert_eq!(h["grips"][0]["hand"], "either");
    }

    #[test]
    fn no_grips_means_not_carryable() {
        assert!(hands_json(&synth(json!([]))).is_none());
        assert!(hands_json(&json!({"Components": []})).is_none());
    }

    /// Real archive survey (skips without it): counts of one-handed / two-handed / without grips per AttachDef type and subtype.
    #[test]
    fn real_archive_grip_survey() {
        use std::collections::BTreeMap;
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = p4k::P4k::open(std::path::Path::new(&path)) else { return };
        let Ok(items) = ItemMaterials::load(&p4k) else { return };
        let Some(d) = items.datacore() else { return };
        let mut tally: BTreeMap<String, [usize; 4]> = BTreeMap::new(); // one, two, uncarryable, mixed
        for (k, i) in ordered_names(&items, RECORD) {
            let f = d.record_file(i).map(|f| f.to_lowercase()).unwrap_or_default();
            if !(f.contains("/scitem/weapons/fps_weapons") || f.contains("/scitem/weapons/throwable") || f.contains("/scitem/consumables") || f.contains("/scitem/weapons/devices")) || is_junk(&k) {
                continue;
            }
            let Ok(rec) = d.record(i) else { continue };
            let Ok(data) = d.record_data(&rec, 4) else { continue };
            let data = data.to_json();
            let att = g(comp(&data, "SAttachableComponentParams"), "AttachDef");
            if !truthy(att) {
                continue;
            }
            let key = format!("{}/{}", s(g(att, "Type")), s(g(att, "SubType")));
            let e = tally.entry(key).or_default();
            match hands_json(&data) {
                Some(h) => {
                    e[(h["hands"].as_u64().unwrap() - 1) as usize] += 1;
                    e[3] += h["mixed"].as_bool().unwrap() as usize;
                }
                None => e[2] += 1,
            }
        }
        for (k, v) in &tally {
            eprintln!("{:40} one {:4} two {:4} none {:4} (mixed {})", k, v[0], v[1], v[2], v[3]);
        }
    }
}
