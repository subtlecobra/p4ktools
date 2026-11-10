//! Human-readable names for object containers (`.socpak`) and the locations built from them.
//!
//! Survey (EMPIRICAL, 2026-11, live archive, 497 loot locations): no localisation key matches a container name. Tried, case
//! insensitively, in `Data/Localization/english/global.ini`: `<name>`, `<name>_name`, `<name>_nme`, `loc_<name>`, `pu_<name>`,
//! `item_name<name>`: 0 hits for the 497 names (the only keys equal to a directory segment are the system names `stanton` / `nyx`,
//! which name a star system, not the container). Station and outpost names exist in the ini only inside mission texts, not under a
//! key derivable from the container. So [`readable`] first asks the caller's localiser for those keys (cheap, and picks up real
//! names should the game ever add them) and otherwise falls back to [`derive`].
//!
//! [`derive`] is INFERRED, a purely textual rule (no per-name patches): lower-case the name, split at `_`, drop the leading
//! `pu` token, expand the abbreviations of [`ABBREV`] (each read off many names in the archive; the meaning follows from the
//! sibling folder names, e.g. `_int` next to `_ext`, `lge`/`med`/`sml` as the size triple), upper-case single letters (variant
//! letters `a`, `b` ...), keep digits (leading zeros dropped) and capitalise every other token. Unknown tokens stay as they are, only
//! capitalised, so nothing is invented.

/// Abbreviation table of container name tokens (INFERRED from the folder and file names they appear in).
pub const ABBREV: &[(&str, &str)] = &[
    ("int", "Interior"),
    ("ext", "Exterior"),
    ("sfce", "Surface"),
    ("spce", "Space"),
    ("lge", "Large"),
    ("med", "Medium"),
    ("sml", "Small"),
    ("ovrly", "Overlay"),
    ("drlct", "Derelict"),
    ("otlw", "Outlaw"),
    ("indy", "Independent"),
    ("occu", "Occupied"),
    ("unoc", "Unoccupied"),
    ("wrhs", "Warehouse"),
    ("wrhse", "Warehouse"),
    ("grge", "Garage"),
    ("trdpst", "Trading Post"),
    ("udrflr", "Underfloor"),
    ("wtn", "Within"),
    ("sa", "Standalone"),
    ("cmpd", "Compound"),
    ("util", "Utility"),
    ("asmbl", "Assembled"),
    ("enctr", "Encounter"),
    ("dung", "Dungeon"),
];

fn cap(t: &str) -> String {
    let mut c = t.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The derived readable form of a container name (INFERRED rule, see the module docs): `ctplr_sfce_mid_med_int_vb` becomes
/// `Ctplr Surface Mid Medium Interior Vb`; single letters are upper-cased, longer variants (`va`, `vb`) only capitalised.
pub fn derive(raw: &str) -> String {
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.strip_suffix(".socpak").or_else(|| base.strip_suffix(".SOCPAK")).unwrap_or(base).to_lowercase();
    let mut out: Vec<String> = vec![];
    for (i, t) in base.split('_').filter(|t| !t.is_empty()).enumerate() {
        if i == 0 && t == "pu" {
            continue;
        }
        if let Some((_, full)) = ABBREV.iter().find(|(a, _)| *a == t) {
            out.push((*full).to_string());
        } else if t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()) {
            out.push(t.to_uppercase());
        } else if t.chars().all(|c| c.is_ascii_digit()) {
            let n = t.trim_start_matches('0');
            out.push(if n.is_empty() { "0".to_string() } else { n.to_string() });
        } else {
            out.push(cap(t));
        }
    }
    if out.is_empty() {
        raw.to_string()
    } else {
        out.join(" ")
    }
}

/// Readable name for a container name: a real localised text when `loc` (given the bare ini key, no `@`) knows one of the keys
/// `<name>`, `<name>_name`, `<name>_nme`, `loc_<name>`, `pu_<name>`, else [`derive`]. The flag is true for a real localised name.
pub fn readable(raw: &str, loc: impl Fn(&str) -> Option<String>) -> (String, bool) {
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.strip_suffix(".socpak").unwrap_or(base).to_lowercase();
    for k in [base.clone(), format!("{base}_name"), format!("{base}_nme"), format!("loc_{base}"), format!("pu_{base}")] {
        if let Some(v) = loc(&k).filter(|v| !v.trim().is_empty()) {
            return (v, true);
        }
    }
    (derive(raw), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_expands_and_cleans() {
        assert_eq!(derive("ctplr_sfce_mid_med_int_vb"), "Ctplr Surface Mid Medium Interior Vb");
        assert_eq!(derive("pu_drlct_habs_sm_int_15_a"), "Derelict Habs Sm Interior 15 A");
        assert_eq!(derive("Data/ObjectContainers/x/hab_a_int_ovrly_indy_a.socpak"), "Hab A Interior Overlay Independent A");
        assert_eq!(derive("orison_hab_platform_014"), "Orison Hab Platform 14");
        assert_eq!(derive("siege_instance_000"), "Siege Instance 0");
    }

    #[test]
    fn derive_never_returns_empty() {
        assert_eq!(derive("pu"), "pu");
        assert_eq!(derive("__"), "__");
    }

    #[test]
    fn readable_prefers_localised_text() {
        let loc = |k: &str| (k == "kareah_name").then(|| "Security Post Kareah".to_string());
        assert_eq!(readable("EA/x/kareah.socpak", loc), ("Security Post Kareah".to_string(), true));
        assert_eq!(readable("securitypostkareah_int", |_| None), ("Securitypostkareah Interior".to_string(), false));
        // an empty text does not count
        assert!(!readable("a_int", |_| Some(" ".into())).1);
    }
}
