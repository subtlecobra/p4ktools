//! Reverse loot index: item class -> the loot tables that can produce it -> the crates / spawn point groups / locations that use them.
//!
//! Built from the forward index (`loot::build`: `locations`, `tables`) with the same tag matching as the pool view, so every INFERRED
//! rule of the forward index applies here too (an item is in an entry's pool when it carries the entry's tags; uniform weights inside
//! a pool).  Nothing here was confirmed against the engine.
//!
//! Likelihood (INFERRED, a rough per-roll figure and not a drop rate):
//! * `table p` = sum over the table's (archetype, entry) pools containing the item of `archetype.chance * entry.chance / pool size`
//!   (one roll of the table; secondary groups such as Rarity, the number of results per archetype and the crate fullness are ignored);
//! * `group p` = `init * share * table p` of one slot (fill probability of the group's spawn point x pick share of the slot x table p);
//! * a location's `p` is the best `group p` found in it;
//! * a location's `exp` (expectancy) is the expected number of table rolls that yield the item when every spawn point of the location is visited
//!   once: the sum over all slots using a table holding the item of `points * init * share * table p` (a spawn point is filled with probability
//!   `init`, picks the slot with probability `share`, rolls the table once).  Result counts per roll, rarity layers and fullness are ignored, so
//!   it is an expected number of hits, not of item copies.
//!
//! Item copies (`exp_items`): `exp` x the number of rolls a crate makes (`lootcount`: the fill volume of the crate divided by the mean stack volume of
//! the table, supplied by the caller as `rolls`; the web layer knows the crate capacities and item volumes).  Rarity layers are not applied here.
//!
//! Memory: classes, tables and crates are interned (u32 ids); a hit is 12 bytes and there is one per (item, entry) pair.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

/// One (loot table, archetype, entry) pool an item is part of, with its per-roll probability.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub table: u32,
    pub arch: u16,
    pub entry: u16,
    pub p: f32,
    /// the entry selects by tags (a slot's pool filter applies to it) and not by an explicit class list (CONFIRMED rule, INFERRED scope)
    pub tag_sel: bool,
}

/// One slot of one spawn point group that uses a loot table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Use {
    pub loc: u32,
    pub ent: u16,
    pub group: u16,
    pub crate_id: u32,
    pub share: f32,
    pub init: f32,
    pub points: u32,
    /// the slot's `fullnessFactorRange`, `totalResultsLimit` and fullness mode (`preventExceed`), inputs of the fill-volume model (`lootcount`)
    pub full: [f32; 2],
    pub limit: f32,
    pub prevent: bool,
    /// 1-based index into `Rev::filters` of the slot's pool filter (0 = none: every item passes)
    pub filter: u16,
}

/// One slot of one spawn point group whose entity class carries the item in its fixed loadout (`lootclass`): present in every crate the slot places.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FixedUse {
    pub loc: u32,
    pub ent: u16,
    pub group: u16,
    pub crate_id: u32,
    pub share: f32,
    pub init: f32,
    pub points: u32,
    /// copies per crate (sum of the loadout entries of this class)
    pub amount: u32,
}

#[derive(Default)]
pub struct Rev {
    /// per class id: the slots whose crate carries it as a fixed loadout item (certain, probability 1 once the slot is picked)
    fixed: Vec<Vec<FixedUse>>,
    classes: Vec<String>,
    ids: HashMap<String, u32>,
    hits: Vec<Vec<Hit>>,
    pub tables: Vec<String>,
    uses: Vec<Vec<Use>>,
    pub crates: Vec<String>,
    /// distinct pool filter expressions of the slots (`loot::filter_passes`)
    pub filters: Vec<Value>,
}

/// A table that can produce the item: `p` = sum of the entries' per-roll probabilities, `entries` = (archetype, entry, p).
#[derive(Debug)]
pub struct TableHit {
    pub table: u32,
    pub p: f64,
    pub entries: Vec<(u16, u16, f64)>,
    /// per entry: it selects by tags (so a slot pool filter can remove the item from it)
    pub tag_sel: Vec<bool>,
}

/// A location where the item can spawn.
#[derive(Debug)]
pub struct LocHit {
    pub loc: u32,
    /// spawn points of the matching groups (each group counted once)
    pub points: u32,
    /// crate class id -> spawn points of the groups that can place it
    pub crates: BTreeMap<u32, u32>,
    /// (entity index, group index) of the matching groups
    pub groups: Vec<(u16, u16)>,
    pub p: f64,
    /// expected number of rolls yielding the item over the whole location (see the module docs)
    pub exp: f64,
    /// expected number of item copies over the whole location with the crates' fill volume applied (`lootcount`; stack sizes not included)
    pub exp_items: f64,
    /// expected copies that are fixed loadout contents (certain once the slot is picked): sum of `points x init x share x amount`; already part of
    /// `exp_items`, and `exp` counts the crates that carry the item
    pub fixed: f64,
}

#[derive(Debug, Default)]
pub struct Where {
    pub tables: Vec<TableHit>,
    pub locs: Vec<LocHit>,
}

impl Rev {
    /// `tables` = the `tables` object of the loot index, `locations` its `locations` array.  `pool(p, n)` returns the full class list of
    /// a pool the table JSON cut at its cap (`q.p`, `q.n` = comma separated tag guids).
    pub fn build(tables: &Value, locations: &Value, pool: &dyn Fn(&str, &str) -> Vec<String>) -> Rev {
        let mut r = Rev::default();
        let mut tix: HashMap<&str, u32> = HashMap::new();
        if let Some(o) = tables.as_object() {
            for (name, t) in o {
                let ti = r.tables.len() as u32;
                r.tables.push(name.clone());
                r.uses.push(vec![]);
                tix.insert(name, ti);
                for (ai, ar) in t["arch"].as_array().into_iter().flatten().enumerate() {
                    let ac = ar["eff_chance"].as_f64().or_else(|| ar["chance"].as_f64()).unwrap_or(0.0);
                    for (ei, en) in ar["body"]["entries"].as_array().into_iter().flatten().enumerate() {
                        let n = en["n"].as_u64().unwrap_or(0) as usize;
                        if n == 0 {
                            continue;
                        }
                        let p = (ac * en["chance"].as_f64().unwrap_or(0.0) / n as f64) as f32;
                        let listed: Vec<&str> = en["items"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
                        let full: Vec<String>;
                        let cls: Vec<&str> = if n > listed.len() && en["q"].is_object() {
                            full = pool(en["q"]["p"].as_str().unwrap_or(""), en["q"]["n"].as_str().unwrap_or(""));
                            full.iter().map(String::as_str).collect()
                        } else {
                            listed
                        };
                        let tag_sel = en["selector"].as_str() != Some("LootArchetypeV3Selector_EntityClasses");
                        let hit = Hit { table: ti, arch: ai as u16, entry: ei as u16, p, tag_sel };
                        for c in cls {
                            let id = r.intern(c);
                            r.hits[id as usize].push(hit);
                        }
                    }
                }
            }
        }
        let mut cix: HashMap<String, u32> = HashMap::new();
        let mut fix: HashMap<String, u16> = HashMap::new();
        for l in locations.as_array().into_iter().flatten() {
            let loc = l["id"].as_u64().unwrap_or(0) as u32;
            for (ent, e) in l["entities"].as_array().into_iter().flatten().enumerate() {
                for (gi, gr) in e["groups"].as_array().into_iter().flatten().enumerate() {
                    let points = gr["points"].as_u64().unwrap_or(0) as u32;
                    if points == 0 {
                        continue; // a group no spawn point maps to cannot place anything (EMPIRICAL: many placeholder containers)
                    }
                    let init = gr["init"].as_f64().unwrap_or(1.0) as f32;
                    for sl in gr["slots"].as_array().into_iter().flatten() {
                        r.add_fixed(sl, loc, ent, gi, points, init, &mut cix);
                        let Some(&ti) = sl["table"].as_str().and_then(|t| tix.get(t)) else { continue };
                        let cname = sl["entity"].as_str().or_else(|| sl["preset"].as_str()).unwrap_or("?");
                        let crate_id = *cix.entry(cname.to_string()).or_insert_with(|| {
                            r.crates.push(cname.to_string());
                            r.crates.len() as u32 - 1
                        });
                        let share = sl["share"].as_f64().unwrap_or(0.0) as f32;
                        let fl = &sl["cons"]["fullness"];
                        let full = [fl[0].as_f64().unwrap_or(0.0) as f32, fl[1].as_f64().unwrap_or(1.0) as f32];
                        let limit = sl["cons"]["total_limit"].as_f64().unwrap_or(0.0) as f32;
                        let prevent = sl["cons"]["fullness_mode"].as_str() == Some("preventExceed");
                        let pf = &sl["cons"]["pool_filter"];
                        let filter = match pf["name"].as_str() {
                            Some(n) if pf["expr"].is_object() => *fix.entry(n.to_string()).or_insert_with(|| {
                                r.filters.push(pf["expr"].clone());
                                r.filters.len() as u16
                            }),
                            _ => 0,
                        };
                        r.uses[ti as usize].push(Use { loc, ent: ent as u16, group: gi as u16, crate_id, share, init, points, full, limit, prevent, filter });
                    }
                }
            }
        }
        r
    }

    /// Registers the fixed loadout items of a slot (`slot.class.fixed`, `lootclass::to_json`) as certain contents of the slot's crate.
    fn add_fixed(&mut self, sl: &Value, loc: u32, ent: usize, gi: usize, points: u32, init: f32, cix: &mut HashMap<String, u32>) {
        let items = sl["class"]["fixed"].as_array();
        if items.is_none_or(|i| i.is_empty()) {
            return;
        }
        let cname = sl["entity"].as_str().or_else(|| sl["preset"].as_str()).unwrap_or("?");
        let crate_id = *cix.entry(cname.to_string()).or_insert_with(|| {
            self.crates.push(cname.to_string());
            self.crates.len() as u32 - 1
        });
        let share = sl["share"].as_f64().unwrap_or(0.0) as f32;
        let mut per: BTreeMap<&str, u32> = BTreeMap::new();
        for it in items.into_iter().flatten() {
            if let Some(c) = it["cls"].as_str() {
                *per.entry(c).or_default() += it["amount"].as_i64().unwrap_or(1).max(1) as u32;
            }
        }
        for (c, amount) in per {
            let id = self.intern(c);
            self.fixed[id as usize].push(FixedUse { loc, ent: ent as u16, group: gi as u16, crate_id, share, init, points, amount });
        }
    }

    fn intern(&mut self, c: &str) -> u32 {
        if let Some(&i) = self.ids.get(c) {
            return i;
        }
        let i = self.classes.len() as u32;
        self.classes.push(c.to_string());
        self.hits.push(vec![]);
        self.fixed.push(vec![]);
        self.ids.insert(c.to_string(), i);
        i
    }

    /// Item classes that appear in at least one pool.
    pub fn classes(&self) -> &[String] {
        &self.classes
    }

    /// Where `cls` can spawn: tables (best first) and locations (most spawn points first).  `None` when the class is in no pool.
    pub fn where_is(&self, cls: &str, rolls: &dyn Fn(&Use, &str) -> f64, pass: &dyn Fn(&Value, &str) -> bool) -> Option<Where> {
        let cid = *self.ids.get(cls)? as usize;
        let hits = &self.hits[cid];
        let fixed = &self.fixed[cid];
        let mut by_table: BTreeMap<u32, TableHit> = BTreeMap::new();
        for h in hits {
            let t = by_table.entry(h.table).or_insert(TableHit { table: h.table, p: 0.0, entries: vec![], tag_sel: vec![] });
            t.p += h.p as f64;
            t.entries.push((h.arch, h.entry, h.p as f64));
            t.tag_sel.push(h.tag_sel);
        }
        let mut locs: BTreeMap<u32, LocHit> = BTreeMap::new();
        let mut seen: HashSet<(u32, u16, u16)> = HashSet::new();
        let mut seen_crate: HashSet<(u32, u16, u16, u32)> = HashSet::new();
        for t in by_table.values() {
            for u in &self.uses[t.table as usize] {
                // the slot's pool filter removes the item from every tag-selected entry; an item only reachable that way is not placed by this slot.
                // The per-roll p is summed over the entries that remain (the pool sizes are those of the unfiltered pools: a filter only shrinks them)
                let passes = u.filter == 0 || pass(&self.filters[u.filter as usize - 1], cls);
                let tp: f64 = if passes { t.p } else { t.entries.iter().zip(&t.tag_sel).filter(|(_, ts)| !**ts).map(|(e, _)| e.2).sum() };
                if tp <= 0.0 {
                    continue;
                }
                let l = locs.entry(u.loc).or_insert(LocHit { loc: u.loc, points: 0, crates: BTreeMap::new(), groups: vec![], p: 0.0, exp: 0.0, exp_items: 0.0, fixed: 0.0 });
                if seen.insert((u.loc, u.ent, u.group)) {
                    l.points += u.points;
                    l.groups.push((u.ent, u.group));
                }
                if seen_crate.insert((u.loc, u.ent, u.group, u.crate_id)) {
                    *l.crates.entry(u.crate_id).or_insert(0) += u.points;
                }
                l.p = l.p.max(u.init as f64 * u.share as f64 * tp);
                l.exp += u.points as f64 * u.init as f64 * u.share as f64 * tp;
                // rolls of the crate (stacks until the fill target, see `lootcount`) x per-roll probability = expected copies (stack size 1)
                l.exp_items += u.points as f64 * u.init as f64 * u.share as f64 * tp * rolls(u, &self.tables[t.table as usize]);
            }
        }
        // fixed loadout contents: certain once the slot is picked (probability 1), so the group p is fill x pick share
        for u in fixed {
            let l = locs.entry(u.loc).or_insert(LocHit { loc: u.loc, points: 0, crates: BTreeMap::new(), groups: vec![], p: 0.0, exp: 0.0, exp_items: 0.0, fixed: 0.0 });
            if seen.insert((u.loc, u.ent, u.group)) {
                l.points += u.points;
                l.groups.push((u.ent, u.group));
            }
            if seen_crate.insert((u.loc, u.ent, u.group, u.crate_id)) {
                *l.crates.entry(u.crate_id).or_insert(0) += u.points;
            }
            let crates = u.points as f64 * u.init as f64 * u.share as f64;
            l.p = l.p.max(u.init as f64 * u.share as f64);
            l.exp += crates;
            l.exp_items += crates * u.amount as f64;
            l.fixed += crates * u.amount as f64;
        }
        let mut tables: Vec<TableHit> = by_table.into_values().collect();
        tables.sort_by(|a, b| b.p.total_cmp(&a.p).then(a.table.cmp(&b.table)));
        let mut locs: Vec<LocHit> = locs.into_values().collect();
        locs.sort_by(|a, b| b.points.cmp(&a.points).then(a.loc.cmp(&b.loc)));
        Some(Where { tables, locs })
    }
}

/// Coarse item kind used by the finder's filter, from the AttachDef `Type` (EMPIRICAL names: `WeaponPersonal`, `Char_Armor_*`,
/// `Char_Clothing_*`, ...).  Unknown types are `other`.
pub fn kind_of(type_name: &str, cls: &str) -> &'static str {
    let t = type_name;
    if t == "WeaponPersonal" {
        "weapon"
    } else if t.starts_with("Char_Armor") {
        "armor"
    } else if t.starts_with("Char_Clothing") || t.starts_with("Char_Accessory") {
        "clothing"
    } else if t == "WeaponAttachment" && (cls.ends_with("_mag") || cls.to_lowercase().contains("magazine")) {
        "ammo"
    } else if t == "WeaponAttachment" {
        "attachment"
    } else if t.contains("Magazine") || t.contains("Ammo") {
        "ammo"
    } else if t.contains("Gadget") || t.contains("Consumable") || t.contains("Medical") {
        "gadget"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data() -> (Value, Value) {
        let tables = json!({
            "LootTable.t1": {"arch": [
                {"chance": 0.5, "body": {"entries": [{"n": 2, "items": ["helm", "vest"], "chance": 1.0}]}},
                {"chance": 0.5, "body": {"entries": [{"n": 3, "items": ["gun"], "chance": 0.5, "q": {"p": "a", "n": ""}}, {"n": 1, "items": ["helm"], "chance": 0.5}]}}
            ]},
            "LootTable.t2": {"arch": [{"chance": 1.0, "body": {"entries": [{"n": 1, "items": ["vest"], "chance": 1.0}]}}]}
        });
        let locs = json!([
            {"id": 0, "entities": [{"groups": [
                {"points": 0, "init": 1.0, "slots": [{"table": "LootTable.t2", "entity": "crate_z", "share": 1.0}]},
                {"points": 4, "init": 0.5, "slots": [{"table": "LootTable.t1", "entity": "crate_a", "share": 0.75}, {"table": "LootTable.t2", "entity": "crate_b", "share": 0.25}]},
                {"points": 2, "init": 1.0, "slots": [{"table": "LootTable.t1", "preset": "preset_c", "share": 1.0}]}]}]},
            {"id": 1, "entities": [{"groups": [{"points": 7, "init": 1.0, "slots": [{"table": "LootTable.t2", "entity": "crate_b", "share": 1.0}, {"table": null}]}]}]}
        ]);
        (tables, locs)
    }

    fn pool(p: &str, _n: &str) -> Vec<String> {
        assert_eq!(p, "a");
        vec!["gun".into(), "helm".into(), "rifle".into()]
    }

    #[test]
    fn per_roll_probability_and_capped_pools() {
        let (t, l) = data();
        let r = Rev::build(&t, &l, &pool);
        // helm: entry (0,0) 0.5*1/2 = .25, entry (1,1) 0.5*0.5/1 = .25 and the capped pool (1,0) re-run through `pool`: 0.5*0.5/3
        let w = r.where_is("helm", &|_, _| 1.0, &|_, _| true).unwrap();
        assert_eq!(w.tables.len(), 1);
        let expect = 0.25 + 0.25 + 0.25 / 3.0;
        assert!((w.tables[0].p - expect).abs() < 1e-6, "{}", w.tables[0].p);
        assert_eq!(w.tables[0].entries.len(), 3);
        // rifle only exists in the re-run pool
        assert!(r.where_is("rifle", &|_, _| 1.0, &|_, _| true).is_some());
        assert!(r.where_is("nothing", &|_, _| 1.0, &|_, _| true).is_none());
    }

    #[test]
    fn locations_count_each_group_once() {
        let (t, l) = data();
        let r = Rev::build(&t, &l, &pool);
        let w = r.where_is("vest", &|_, _| 2.0, &|_, _| true).unwrap();
        // vest is in t1 and t2; location 0 uses both in group 0 (counted once: 4 points) and t1 in group 1 (2 points); location 1 has 7
        assert_eq!(w.locs.iter().map(|x| (x.loc, x.points)).collect::<Vec<_>>(), vec![(1, 7), (0, 6)]);
        let l0 = &w.locs[1];
        assert_eq!(l0.groups, vec![(0, 1), (0, 2)]);
        assert_eq!(l0.crates.len(), 3);
        // best group p of location 0: g0/t1 0.5*0.75*0.25, g0/t2 0.5*0.25*1, g1/t1 1*1*0.25
        assert!((l0.p - 0.25).abs() < 1e-6);
        assert!((w.locs[0].p - 1.0).abs() < 1e-6);
        assert_eq!(w.tables[0].table, 1, "best table first");
        // expectancy: location 0 = 4*0.5*0.75*0.25 + 4*0.5*0.25*1 + 2*1*1*0.25, location 1 = 7*1*1*1
        assert!((l0.exp - 1.375).abs() < 1e-6, "{}", l0.exp);
        assert!((w.locs[0].exp - 7.0).abs() < 1e-6);
        // copies = expectancy x rolls per crate (2.0 in this test)
        assert!((l0.exp_items - 2.75).abs() < 1e-6 && (w.locs[0].exp_items - 14.0).abs() < 1e-6);
    }

    #[test]
    fn slot_pool_filter_hides_tag_selected_items_only() {
        let tables = json!({"LootTable.t": {"arch": [{"chance": 1.0, "body": {"entries": [
            {"n": 1, "items": ["gun"], "chance": 0.5},
            {"n": 1, "items": ["boxed"], "chance": 0.5, "selector": "LootArchetypeV3Selector_EntityClasses"}]}}]}});
        let slot = |f: Value| json!({"table": "LootTable.t", "entity": "c", "share": 1.0, "cons": {"pool_filter": f}});
        let flt = json!({"name": "F", "expr": {"t": "tags", "pos": [], "neg": ["ev"]}});
        let locs = json!([{"id": 0, "entities": [{"groups": [{"points": 1, "init": 1.0, "slots": [slot(flt)]}]}]},
                          {"id": 1, "entities": [{"groups": [{"points": 1, "init": 1.0, "slots": [slot(Value::Null)]}]}]}]);
        let r = Rev::build(&tables, &locs, &|_, _| vec![]);
        assert_eq!(r.filters.len(), 1);
        let reject = |_: &Value, _: &str| false;
        // "gun" is tag-selected: the filtered slot (location 0) drops it, the unfiltered one (location 1) keeps it
        let w = r.where_is("gun", &|_, _| 1.0, &reject).unwrap();
        assert_eq!(w.locs.iter().map(|l| l.loc).collect::<Vec<_>>(), vec![1]);
        // "boxed" comes from an explicit class list: the filter does not apply
        let w = r.where_is("boxed", &|_, _| 1.0, &reject).unwrap();
        assert_eq!(w.locs.len(), 2);
        assert!((w.locs[0].p - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fixed_loadout_items_are_certain_and_listed_without_a_pool() {
        let slot = |share: f64| {
            json!({"table": null, "entity": "rack", "share": share, "class": {"fixed": [
                {"cls": "rifle", "amount": 1, "port": "a", "via": "loadout"}, {"cls": "rifle", "amount": 2, "port": "b", "via": "loadout"}, {"cls": "mag", "amount": 1, "port": "", "via": "loadout inventory"}]}})
        };
        let locs = json!([{"id": 3, "entities": [{"groups": [
            {"points": 0, "init": 1.0, "slots": [slot(1.0)]},
            {"points": 4, "init": 0.5, "slots": [slot(0.5), {"table": null, "entity": "bare", "share": 0.5}]}]}]}]);
        let r = Rev::build(&json!({}), &locs, &|_, _| vec![]);
        assert!(r.classes().contains(&"rifle".to_string()) && r.classes().contains(&"mag".to_string()));
        let w = r.where_is("rifle", &|_, _| 9.0, &|_, _| true).unwrap();
        assert!(w.tables.is_empty());
        let l = &w.locs[0];
        // 4 points x fill 0.5 x pick share 0.5 = 1 crate carries it; the two entries of the same class add up to 3 copies per crate
        assert_eq!((l.loc, l.points, l.crates.len()), (3, 4, 1));
        assert!((l.p - 0.25).abs() < 1e-9 && (l.exp - 1.0).abs() < 1e-9 && (l.fixed - 3.0).abs() < 1e-9 && (l.exp_items - 3.0).abs() < 1e-9);
        assert!(r.where_is("bare", &|_, _| 1.0, &|_, _| true).is_none());
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("WeaponPersonal", "x"), "weapon");
        assert_eq!(kind_of("Char_Armor_Torso", "x"), "armor");
        assert_eq!(kind_of("Char_Clothing_Hat", "x"), "clothing");
        assert_eq!(kind_of("WeaponAttachment", "x_scope"), "attachment");
        assert_eq!(kind_of("WeaponAttachment", "gmni_sniper_01_mag"), "ammo");
        assert_eq!(kind_of("AmmoBox", "x"), "ammo");
        assert_eq!(kind_of("FPS_Consumable", "x"), "gadget");
        assert_eq!(kind_of("Misc", "x"), "other");
    }
}
