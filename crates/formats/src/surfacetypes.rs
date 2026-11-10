//! Surface types (Libs/MaterialEffects/SurfaceTypes.xml, CryXmlB): the collision classes a material's SurfaceType gives its faces.
//!
//! A .mtl sub-material names a surface type (`SurfaceType="shield_human"`); the surface type's `<Physics>` element says what the
//! faces collide with.  CONFIRMED against the engine's surface type loader:
//!
//! * `coll_types` -> a u64 collision-type mask at ISurfaceType+0xc0.  Empty / absent: 1 << 63 (a sentinel: "not specified").
//!   Present: the mask starts at 1 << 32 (float) and every comma-separated name ORs its bit in, or clears it when prefixed with
//!   '-' or '~' (numbers set 1 << n).  Names: default 0x1, all 0x7ff, player 0x2, vehicle 0x8, explosion 0x4, ray 0x400, float
//!   1 << 32, water 1 << 32, shield 0x40.  These are the engine's geom_colltype bits.
//! * `no_collide="1"` -> flags |= 4, `vehicle_only_collisions="1"` -> flags |= 8, `can_shatter` -> 0x10.
//! * A surface whose mask has 0x40 OR whose *name* contains "shield" gets mask |= 0x40 and flags |= 0x40.
//!
//! So `shield_human` (`coll_types="-float,shield"`) has the mask 0x40: it collides with the shield class only, not with players
//! (0x2) or vehicles (0x8).  EMPIRICAL (live archive): the four shield_* surfaces are the only ones with coll_types,
//! `mat_no_collide` the only one with no_collide, none uses vehicle_only_collisions.
//!
//! INFERRED, not located in the engine: that the physics applies the mask per triangle through the triangle's
//! surface id, so that the player passes through shield faces.  A surface without coll_types keeps the geometry's own flags.

use std::collections::HashMap;

use crate::cryxml;
use crate::error::*;

pub const COLL_NAMES: [(&str, u64); 9] = [
    ("default", 0x1),
    ("all", 0x7FF),
    ("player", 0x2),
    ("vehicle", 0x8),
    ("explosion", 0x4),
    ("ray", 0x400),
    ("float", 1 << 32),
    ("water", 1 << 32),
    ("shield", 0x40),
];
pub const COLL_PLAYER: u64 = 0x2;
pub const COLL_SHIELD: u64 = 0x40;
pub const COLL_UNSET: u64 = 1 << 63;
pub const SF_NO_COLLIDE: u32 = 0x4;
pub const SF_VEHICLE_ONLY: u32 = 0x8;
pub const SF_CAN_SHATTER: u32 = 0x10;
pub const SF_SHIELD: u32 = 0x40;

#[derive(Debug, Clone, Copy)]
pub struct Surface {
    pub coll: u64,
    pub flags: u32,
}

/// The engine's u64 mask for a coll_types string (see the module docs); COLL_UNSET for an empty one.
pub fn parse_coll_types(text: &str) -> u64 {
    if text.is_empty() {
        return COLL_UNSET;
    }
    let b = text.as_bytes();
    let mut mask: u64 = 1 << 32;
    // numbers first (the engine scans digit runs), then the names by substring search, both honouring a '-' / '~' prefix
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let n: u64 = text[i..j].parse().unwrap_or(u64::MAX);
            let bit = 1u64 << (n & 63);
            mask = if i > 0 && (b[i - 1] == b'-' || b[i - 1] == b'~') { mask & !bit } else { mask | bit };
            i = j;
        } else {
            i += 1;
        }
    }
    for (name, bit) in COLL_NAMES {
        // strstr: the first occurrence decides, as in the engine
        if let Some(k) = text.find(name) {
            mask = if k > 0 && (b[k - 1] == b'-' || b[k - 1] == b'~') { mask & !bit } else { mask | bit };
        }
    }
    mask
}

fn flag(a: &HashMap<String, String>, k: &str) -> bool {
    let v = a.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("0");
    !(v.is_empty() || v == "0")
}

/// {coll, flags} of one `<SurfaceType>` from its name and its `<Physics>` attributes.
pub fn surface_entry(name: &str, a: &HashMap<String, String>) -> Surface {
    let mut coll = parse_coll_types(a.get("coll_types").map(String::as_str).unwrap_or(""));
    let mut flags = 0;
    if flag(a, "no_collide") {
        flags |= SF_NO_COLLIDE;
    }
    if flag(a, "vehicle_only_collisions") {
        flags |= SF_VEHICLE_ONLY;
    }
    if flag(a, "can_shatter") {
        flags |= SF_CAN_SHATTER;
    }
    if coll & COLL_SHIELD != 0 || name.contains("shield") {
        // the engine's strstr on the name is case-sensitive
        coll |= COLL_SHIELD;
        flags |= SF_SHIELD;
    }
    Surface { coll, flags }
}

/// {surface name (lower case): {coll, flags}} from the bytes of SurfaceTypes.xml (CryXmlB; plain text XML is not supported here).
pub fn parse_surface_types(data: &[u8]) -> Result<HashMap<String, Surface>> {
    let root = cryxml::parse(data)?;
    let mut out = HashMap::new();
    let mut found = vec![];
    root.iter("SurfaceType", &mut found);
    for s in found {
        let name = s.get("name").unwrap_or("");
        let attrs: HashMap<String, String> = s.find("Physics").map(|p| p.attrs.iter().cloned().collect()).unwrap_or_default();
        out.insert(name.to_lowercase(), surface_entry(name, &attrs));
    }
    Ok(out)
}

/// False when faces of this surface type do not stop the player: no_collide, vehicle-only, or a coll_types mask without the
/// player bit.  An unknown surface (None) or one without coll_types keeps the geometry's flags -> true.
pub fn player_collides(entry: Option<&Surface>) -> bool {
    let Some(e) = entry else { return true };
    if e.flags & (SF_NO_COLLIDE | SF_VEHICLE_ONLY) != 0 {
        return false;
    }
    if e.coll & COLL_UNSET != 0 {
        return true;
    }
    e.coll & COLL_PLAYER != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shield_surface_blocks_nothing_for_the_player() {
        let mut a = HashMap::new();
        a.insert("coll_types".to_string(), "-float,shield".to_string());
        let s = surface_entry("shield_human", &a);
        assert_eq!(s.coll & COLL_SHIELD, COLL_SHIELD);
        assert_eq!(s.coll & (1 << 32), 0);
        assert!(!player_collides(Some(&s)));
        assert!(player_collides(None));
        assert!(player_collides(Some(&surface_entry("metal", &HashMap::new()))));
    }

    #[test]
    fn no_collide_flag() {
        let mut a = HashMap::new();
        a.insert("no_collide".to_string(), "1".to_string());
        assert!(!player_collides(Some(&surface_entry("mat_no_collide", &a))));
    }
}
