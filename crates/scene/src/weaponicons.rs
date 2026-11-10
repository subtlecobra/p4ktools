//! Per-weapon icons (the silhouettes of the loadout screen and of the game feed / kill feed).
//!
//! * CONFIRMED (Game2.dcb): an item's `StaticEntityClassData[].displayIcon` is a `ui/textures/ea/loadouticons/<name>_64.tif` path (the archive
//!   holds `Data/UI/Textures/EA/LoadoutIcons/<name>_64.dds`, 64x64 BC3).  56 of the 65 listed personal weapons have one.
//! * CONFIRMED (archive): `Data/UI/Textures/EA/GameFeedIcons/<name>.dds` holds a second, wide (128x32 BC3) silhouette per weapon (22 of them).
//!   No DataCore field or Scaleform file names them, so the pairing with the loadout icon is by file name only: INFERRED (name with `_64`
//!   dropped, case / underscores ignored).

use std::collections::HashMap;
use std::sync::OnceLock;

use p4k_formats::dds;
use p4k_formats::png::png_rgba;
use p4k::{Entry, P4k};
use regex::Regex;
use serde_json::Value;

use crate::textures::{texture_base, DdsTexture};

pub const FEED_DIR: &str = "ui/textures/ea/gameFeedIcons/";

/// The `displayIcon` path of an item record (dict from `ItemStats.record`), or "".
pub fn display_icon(record: &Value) -> String {
    if let Some(Value::Array(list)) = record.get("StaticEntityClassData") {
        for s in list {
            if let Some(i) = s.get("displayIcon").and_then(Value::as_str) {
                if !i.is_empty() {
                    return i.to_string();
                }
            }
        }
    }
    String::new()
}

fn norm(name: &str) -> String {
    name.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect()
}

/// Names (lower case, no extension) the wide kill-feed icon of a loadout icon may have: the file name without its `_64` suffix.
pub fn feed_candidates(icon: &str) -> Vec<String> {
    let t = texture_base(icon);
    let base = t.rsplit('/').next().unwrap_or("");
    let base = base.strip_suffix("_64").unwrap_or(base);
    vec![base.to_string()]
}

/// {normalised name: entry index} of the feed icons in archive order (a repeated key keeps its first position and its last entry).
pub struct FeedIndex {
    keys: Vec<String>,
    map: HashMap<String, usize>,
}

impl FeedIndex {
    pub fn build(p4k: &P4k) -> FeedIndex {
        let pre = format!("data/{}", FEED_DIR.to_lowercase());
        let mut idx = FeedIndex { keys: vec![], map: HashMap::new() };
        for (i, e) in p4k.entries.iter().enumerate() {
            let low = e.name.to_lowercase();
            if low.starts_with(&pre) && low.ends_with(".dds") && !low.starts_with(&format!("{}game_feed", pre)) {
                let Some(stem) = e.name.get(pre.len()..e.name.len() - 4) else { continue };
                let k = norm(stem);
                if idx.map.insert(k.clone(), i).is_none() {
                    idx.keys.push(k);
                }
            }
        }
        idx
    }
}

/// Entry of the kill-feed icon paired with a loadout icon (INFERRED by name, tolerant of case, underscores and trailing numbers), or None.
pub fn find_feed<'a>(p4k: &'a P4k, idx: &FeedIndex, icon: &str) -> Option<&'a Entry> {
    static TRAIL: OnceLock<Regex> = OnceLock::new();
    let trail = TRAIL.get_or_init(|| Regex::new(r"\d+$").unwrap());
    let want: Vec<String> = feed_candidates(icon).iter().map(|c| norm(c)).collect();
    for w in &want {
        let stripped = trail.replace(w, "").into_owned();
        for key in [w.clone(), stripped] {
            if let Some(&i) = idx.map.get(&key) {
                return Some(&p4k.entries[i]);
            }
        }
    }
    // the loosest prefix match last
    for w in &want {
        for k in &idx.keys {
            if w.chars().count() > 8 && (w.starts_with(k.as_str()) || k.starts_with(w.as_str())) {
                return Some(&p4k.entries[idx.map[k]]);
            }
        }
    }
    None
}

/// PNG bytes of the largest mip of a BC1/2/3 icon entry, or None when it cannot be read or decoded.
pub fn icon_png(p4k: &P4k, ent: &Entry) -> Option<Vec<u8>> {
    let tex = DdsTexture::new(p4k, ent, false).ok()?;
    if !dds::DECODABLE.contains(&tex.format.name) {
        return None;
    }
    let data = tex.level(0)?;
    let (w, h) = tex.dims[0];
    let px = dds::decode(tex.format.name, &data, w, h).ok()?;
    Some(png_rgba(w as u32, h as u32, &px))
}

/// PNG of the loadout icon named by a `displayIcon` path (.tif in the record, .dds in the archive), or None.
pub fn loadout_png(p4k: &P4k, icon: &str) -> Option<Vec<u8>> {
    if icon.is_empty() {
        return None;
    }
    let ent = p4k.find_path(&format!("{}.dds", texture_base(icon)), ".dds")?;
    icon_png(p4k, ent)
}

/// Icons of one archive; the feed index is built on first use.
pub struct WeaponIcons<'a> {
    p4k: &'a P4k,
    feed: OnceLock<FeedIndex>,
}

impl<'a> WeaponIcons<'a> {
    pub fn new(p4k: &'a P4k) -> Self {
        WeaponIcons { p4k, feed: OnceLock::new() }
    }

    pub fn find_feed(&self, icon: &str) -> Option<&'a Entry> {
        let idx = self.feed.get_or_init(|| FeedIndex::build(self.p4k));
        find_feed(self.p4k, idx, icon)
    }

    /// PNG of the wide kill-feed icon paired with a loadout icon, or None.
    pub fn feed_png(&self, icon: &str) -> Option<Vec<u8>> {
        if icon.is_empty() {
            return None;
        }
        icon_png(self.p4k, self.find_feed(icon)?)
    }

    pub fn loadout_png(&self, icon: &str) -> Option<Vec<u8>> {
        loadout_png(self.p4k, icon)
    }
}
