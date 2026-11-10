//! The loot index: which locations hold loot crates, which crates spawn there and which items can come out of them.
//! The short version of the survey (all EMPIRICAL on the live 4.x archive, nothing here was confirmed against the engine):
//!
//! * A **location** is an object container (`.socpak`) whose entity list holds an entity with a `HarvestableComponent`
//!   (`looseSubConfigBase` = `SubHarvestableMultiConfigRef{multiConfigRef}` or `SubHarvestableConfigRef{subConfigRef}`, a GUID of a
//!   `SubHarvestableMultiConfigRecord` / `SubHarvestableConfigRecord`).  Those entities are `HarvestableStreamableObjectContainer`,
//!   `HarvestableObjectContainer` or `LocationHarvestableObjectContainer`; their `EntityComponentObjectContainer.objectContainer` is a
//!   child container whose `metadata/entitycomponentslotspawnpoint.xml` lists the **spawn points** (`relativeTransform "Q : w,x,y,z |
//!   T : x,y,z"`, `tags` = comma separated Tag GUIDs).
//! * The config record decides what appears at a spawn point: a multi config has `taggedConfigs[]{name, tagList.tags, subConfig}` and a
//!   `defaultConfig`; a sub config has `initialSlotsProbability` and `subHarvestables[]` slots `{harvestable (HarvestablePreset = the
//!   crate), relativeProbability, lootConfig{lootTable | lootTableV3, lootConstraints}}`.  INFERRED (names only): a spawn point takes the
//!   tagged config whose tags are all carried by the point (ancestors included), else the default config; it is filled with probability
//!   `initialSlotsProbability`; one slot is picked by `relativeProbability`.
//! * A **loot table** (`LootTable`, `LootTableV3Record`) weights **archetypes**; an archetype weights **entries**; an entry selects items
//!   by Tag (`LootArchetypeEntry_Primary.tag` + `additionalTags`, V3: `LootArchetypeV3Selector_Tags`) or by explicit class list
//!   (`LootArchetypeV3Selector_EntityClasses`).  INFERRED: an item matches when it carries every positive tag (or a child tag of it), none
//!   of the negative / excluded ones.  Secondary groups (e.g. "Rarity") are listed with their weights and not applied to the pools.
//!
//! Static data only; nothing says where a crate is in a running game, only where the data allows one.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use p4k_formats::cryxml::{self, Element};
use p4k_formats::dcb::Dcb;
use p4k_formats::socpak::{iter_all, SocZip};
use p4k::P4k;
use serde_json::{json, Map, Value};

use crate::dynv::{arr, f, g, s, truthy};
use crate::itemmaterials::ItemMaterials;
use crate::lootclass::{self, ClassFill, Resolve};

/// Items listed per pool entry (the full count is kept in `n`).
pub const POOL_CAP: usize = 400;
/// Items listed per secondary-tag sub pool.
pub const SUB_CAP: usize = 120;
const NULL_GUID: &str = "00000000-0000-0000-0000-000000000000";
const ENTITY: &str = "EntityClassDefinition.";

fn ref_guid(v: &Value) -> Option<&str> {
    let a = v.as_array()?;
    if a.len() == 3 && a[0] == "ref" {
        a[2].as_str().filter(|g| *g != NULL_GUID)
    } else {
        None
    }
}

/// An enum value as text (the DataCore JSON carries enums as strings).
fn enum_str(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| if v.is_null() { String::new() } else { v.to_string() })
}

/// Item -> tag set index with the pool query.
///
/// The set an item is filed under is its explicit tags PLUS ALL THEIR ANCESTORS (CONFIRMED against the engine; the lootable tag and its descendants
/// are removed first, see `TagMode`), so a parent-tag selector (e.g. "Type") matches an item that only lists a child tag, and a negative parent tag
/// excludes any item with a descendant.  A pool is "all positive tags and none of the negative tags", exact membership in that set.
pub struct Tags {
    names: HashMap<String, String>,
    /// entity class (without the record prefix) -> its tag set (explicit + ancestors)
    item_tags: Vec<(String, HashSet<String>)>,
    by_tag: HashMap<String, Vec<u32>>,
    by_class: HashMap<String, u32>,
    /// tagged entity classes left out because their record has no `inventoryOccupancyVolume` (CONFIRMED: lootable classes with zero / missing
    /// volume are excluded from the item database)
    pub excluded_no_volume: usize,
    /// guid -> full name (`Parent.Child`, parent chain joined by dots); empty for hand-built tag sets
    full: HashMap<String, String>,
    /// classes dropped by admission: no lootable tag, or nothing left after removing it (CONFIRMED)
    pub excluded_not_lootable: usize,
    /// whether `LootGeneration.CanGenerateAsLoot` was found (admission only runs when it was)
    pub loot_tag_found: bool,
}

impl Tags {
    pub fn new(names: HashMap<String, String>, items: Vec<(String, Vec<String>)>) -> Tags {
        let mut t = Tags { names, item_tags: vec![], by_tag: HashMap::new(), by_class: HashMap::new(), excluded_no_volume: 0, full: HashMap::new(), excluded_not_lootable: 0, loot_tag_found: false };
        for (cls, tags) in items {
            let set: HashSet<String> = tags.into_iter().collect();
            let idx = t.item_tags.len() as u32;
            for g in &set {
                t.by_tag.entry(g.clone()).or_default().push(idx);
            }
            t.by_class.insert(cls.clone(), idx);
            t.item_tags.push((cls, set));
        }
        t
    }

    pub fn name(&self, guid: &str) -> String {
        self.names.get(guid).cloned().unwrap_or_else(|| guid.to_string())
    }

    /// The full name of a tag (`Parent.Child`), the short name when the tree is unknown.
    pub fn full_name(&self, guid: &str) -> String {
        self.full.get(guid).cloned().unwrap_or_else(|| self.name(guid))
    }

    /// The tag guids of an item class (explicit + ancestors).
    pub fn tags_of(&self, cls: &str) -> Option<&HashSet<String>> {
        self.by_class.get(cls).map(|&i| &self.item_tags[i as usize].1)
    }

    /// Whether the item carries a tag whose name contains `needle` (lower case), e.g. `rare`, `armor` (explicit tags and their ancestors).
    pub fn item_has_tag_like(&self, cls: &str, needle: &str) -> bool {
        self.tags_of(cls).is_some_and(|t| t.iter().any(|g| self.names.get(g).is_some_and(|n| n.to_lowercase().contains(needle))))
    }

    /// Whether the item carries the tag called exactly `name`.
    pub fn item_has_tag_named(&self, cls: &str, name: &str) -> bool {
        self.tags_of(cls).is_some_and(|t| t.iter().any(|g| self.names.get(g).is_some_and(|n| n == name)))
    }

    /// Entity classes carrying every tag of `pos` and none of `neg` (sorted).  An empty `pos` selects nothing (INFERRED: an archetype /
    /// SpawnWith selector without positive tags returns nothing).
    pub fn pool(&self, pos: &[String], neg: &[String]) -> Vec<&str> {
        let Some(first) = pos.iter().map(|p| self.by_tag.get(p).map_or(&[][..], |v| &v[..])).min_by_key(|v| v.len()) else { return vec![] };
        let mut out: Vec<&str> = first
            .iter()
            .map(|&i| &self.item_tags[i as usize])
            .filter(|(_, ext)| pos.iter().all(|p| ext.contains(p)) && !neg.iter().any(|n| ext.contains(n)))
            .map(|(c, _)| c.as_str())
            .collect();
        out.sort_unstable();
        out
    }

    /// The indexed class called `name` (None when it is untagged or was excluded for having no volume).
    pub fn class_ref(&self, name: &str) -> Option<&str> {
        self.by_class.get(name).map(|&i| self.item_tags[i as usize].0.as_str())
    }

    /// Number of indexed item classes.
    pub fn n_items(&self) -> usize {
        self.item_tags.len()
    }
}

/// A pool filter as the index exports it: `{"t":"tags","pos":[guid],"neg":[guid]}`, `{"t":"seq","f":[{"or":bool,"x":expr}]}`, `{"t":"unknown"}`.
/// Rules (CONFIRMED): no filter = every item passes;
/// Tags = all positive tags and none of the negative ones (exact membership); Sequence folds left to right (the first filter seeds, each
/// later one is ANDed when its mode is 0 / `Cumulative`, ORed otherwise; the enum names `Cumulative` / `Additive` for the values 0 / 1 are
/// INFERRED); an empty sequence or an unknown type rejects everything (INFERRED).
pub fn filter_passes(expr: &Value, tags: &HashSet<String>) -> bool {
    match s(g(expr, "t")) {
        "tags" => {
            let has = |v: &Value| arr(v).iter().filter_map(|x| x.as_str()).map(|x| tags.contains(x)).collect::<Vec<_>>();
            has(g(expr, "pos")).iter().all(|b| *b) && !has(g(expr, "neg")).iter().any(|b| *b)
        }
        "seq" => {
            let mut acc: Option<bool> = None;
            for st in arr(g(expr, "f")) {
                let v = filter_passes(g(st, "x"), tags);
                acc = Some(match acc {
                    None => v,
                    Some(a) if g(st, "or").as_bool().unwrap_or(false) => a || v,
                    Some(a) => a && v,
                });
            }
            acc.unwrap_or(false)
        }
        _ => false,
    }
}

/// Which tagged config a spawn point takes (CONFIRMED): configs without tags are ignored, the rest are stable-sorted by tag count descending
/// (explicit counts), the first whose tags are ALL
/// among the point's EXPLICIT tags wins (most specific first, authoring order breaks ties; neither side is ancestor-expanded), else the default
/// config.  Tags compare by name string (here: full names).  `groups` = (is default, tag names).
/// The names a spawn point offers to the matcher: its explicit tags as full names, minus the two engine markers ([`POINT_ENGINE_TAGS`]).
pub fn point_tag_names(tags: &Tags, point_tags: &[String]) -> HashSet<String> {
    point_tags.iter().map(|t| tags.full_name(t)).filter(|n| !POINT_ENGINE_TAGS.contains(&n.as_str())).collect()
}

pub fn pick_group(groups: &[(bool, Vec<String>)], point: &HashSet<String>) -> Option<usize> {
    let mut order: Vec<usize> = (0..groups.len()).filter(|&i| !groups[i].0 && !groups[i].1.is_empty()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(groups[i].1.len()));
    order.into_iter().find(|&i| groups[i].1.iter().all(|t| point.contains(t))).or_else(|| groups.iter().position(|x| x.0))
}

/// Marker tags the engine REMOVES from an entity's copied tags when it builds the slot tag list (CONFIRMED), so a config requiring
/// `Harvestable.SpawnPoint` can never match a tagpoint slot.  Whether the entity's copied tags contain class-level tags is UNRESOLVED
/// (INFERRED: the point's own tags only).
pub const POINT_ENGINE_TAGS: [&str; 2] = ["Harvestable.SpawnPoint", "EntityArea.TrackInArea.ActionArea"];

fn pool_json(cls: &[&str]) -> Value {
    json!({"n": cls.len(), "items": cls.iter().take(POOL_CAP).collect::<Vec<_>>()})
}

/// `q` of a pool whose list was cut at the cap: the tag query (comma separated positive / negative tag guids) that `/loot/pool.json` re-runs
/// to list every item (`Tags::pool`).  Absent when the list is complete.
fn pool_query(j: &mut Value, n: usize, cap: usize, pos: &[String], neg: &[String]) {
    if n > cap {
        j["q"] = json!({"p": pos.join(","), "n": neg.join(",")});
    }
}

struct Reader<'a> {
    items: &'a ItemMaterials,
    d: &'a Dcb,
    tags: Tags,
    /// entity class name -> its loot config and fixed loadout (`lootclass`), read once per class
    fills: RefCell<HashMap<String, Rc<ClassFill>>>,
}

impl Resolve for Reader<'_> {
    fn class_of(&self, r: &Value) -> Option<String> {
        let n = self.rname(ref_guid(r)?)?;
        n.strip_prefix(ENTITY).map(String::from)
    }
    fn loadout_record(&self, r: &Value) -> Option<Value> {
        self.rec(ref_guid(r)?, 12).map(|x| x.1)
    }
    fn record_name(&self, r: &Value) -> Option<String> {
        self.rname(ref_guid(r)?)
    }
}

impl<'a> Reader<'a> {
    fn rec(&self, guid: &str, depth: usize) -> Option<(String, Value)> {
        let i = self.items.guid_record(guid)?;
        let rec = self.d.record(i).ok()?;
        let v = self.d.record_data(&rec, depth).ok()?.to_json();
        Some((self.d.record_name(i).ok()?.into_owned(), v))
    }

    fn rname(&self, guid: &str) -> Option<String> {
        let i = self.items.guid_record(guid)?;
        Some(self.d.record_name(i).ok()?.into_owned())
    }

    fn short(name: &str) -> &str {
        name.split_once('.').map_or(name, |x| x.1)
    }

    fn guids(&self, v: &Value) -> Vec<String> {
        arr(v).iter().filter_map(ref_guid).map(String::from).collect()
    }

    /// Pool entries of a V1 archetype record (`LootArchetype`).
    fn archetype_v1(&self, r: &Value) -> Value {
        let excl = self.guids(g(g(r, "excludedTags"), "tags"));
        let mut entries = vec![];
        let es = arr(g(g(r, "primaryOrGroup"), "entries"));
        let total: f64 = es.iter().map(|e| f(g(e, "weight"))).sum();
        for e in es {
            let mut pos: Vec<String> = ref_guid(g(e, "tag")).map(String::from).into_iter().collect();
            pos.extend(self.guids(g(g(e, "additionalTags"), "positiveTags")));
            let mut neg = self.guids(g(g(e, "additionalTags"), "negativeTags"));
            neg.extend(excl.iter().cloned());
            let pool = self.tags.pool(&pos, &neg);
            let mut j = pool_json(&pool);
            pool_query(&mut j, pool.len(), POOL_CAP, &pos, &neg);
            j["name"] = json!(s(g(e, "name")));
            j["weight"] = json!(f(g(e, "weight")));
            j["chance"] = json!(if total > 0.0 { f(g(e, "weight")) / total } else { 0.0 });
            j["tags"] = json!(pos.iter().map(|t| self.tags.name(t)).collect::<Vec<_>>());
            j["not"] = json!(neg.iter().map(|t| self.tags.name(t)).collect::<Vec<_>>());
            j["opt"] = json!(self.optional(g(e, "optionalData")));
            // secondary groups narrow the pool by one more tag each (INFERRED: the picked secondary tag is ANDed with the entry)
            let sec: Vec<Value> = arr(g(r, "secondaryOrGroups"))
                .iter()
                .map(|gr| {
                    let ses = arr(g(gr, "entries"));
                    let st: f64 = ses.iter().map(|x| f(g(x, "weight"))).sum();
                    let subs: Vec<Value> = ses
                        .iter()
                        .filter_map(|x| {
                            let t = ref_guid(g(x, "tag"))?;
                            let mut p2 = pos.clone();
                            p2.push(t.to_string());
                            let sub = self.tags.pool(&p2, &neg);
                            let mut sj = json!({"tag": self.tags.name(t), "weight": f(g(x, "weight")), "chance": if st > 0.0 { f(g(x, "weight")) / st } else { 0.0 },
                                                "n": sub.len(), "items": sub.iter().take(SUB_CAP).collect::<Vec<_>>()});
                            pool_query(&mut sj, sub.len(), SUB_CAP, &p2, &neg);
                            Some(sj)
                        })
                        .collect();
                    json!({"group": s(g(gr, "groupName")), "entries": subs})
                })
                .collect();
            j["sec"] = json!(sec);
            entries.push(j);
        }
        json!({"entries": entries})
    }

    /// Pool entries of a V3 archetype (`LootArchetypeV3`, inline or the one inside a `LootArchetypeV3Record`).
    fn archetype_v3(&self, a: &Value) -> Value {
        let es = arr(g(a, "entries"));
        let total: f64 = es.iter().map(|e| f(g(e, "weight"))).sum();
        let mut entries = vec![];
        for e in es {
            let sel = g(e, "selector");
            let mut guids: Option<(Vec<String>, Vec<String>)> = None;
            let (pool, tags, not): (Vec<&str>, Vec<String>, Vec<String>) = match s(g(sel, "__struct__")) {
                "LootArchetypeV3Selector_Tags" => {
                    let pos = self.guids(g(g(sel, "tags"), "positiveTags"));
                    let neg = self.guids(g(g(sel, "tags"), "negativeTags"));
                    let r = (self.tags.pool(&pos, &neg), pos.iter().map(|t| self.tags.name(t)).collect(), neg.iter().map(|t| self.tags.name(t)).collect());
                    guids = Some((pos, neg));
                    r
                }
                "LootArchetypeV3Selector_EntityClasses" => {
                    let mut names = vec![];
                    collect_refs(sel, &mut names);
                    let cls: Vec<&str> = names.iter().filter_map(|g| self.class_name(g)).collect();
                    (cls, vec![], vec![])
                }
                _ => (vec![], vec![], vec![]),
            };
            let mut j = pool_json(&pool);
            if let Some((pp, nn)) = &guids {
                pool_query(&mut j, pool.len(), POOL_CAP, pp, nn);
            }
            j["name"] = json!(s(g(e, "name")));
            j["weight"] = json!(f(g(e, "weight")));
            j["chance"] = json!(if total > 0.0 { f(g(e, "weight")) / total } else { 0.0 });
            j["tags"] = json!(tags);
            j["not"] = json!(not);
            j["selector"] = json!(s(g(sel, "__struct__")));
            j["opt"] = json!(self.optional(g(e, "optionalData")));
            entries.push(j);
        }
        json!({"entries": entries})
    }

    fn tag_names(&self, v: &Value) -> Vec<String> {
        self.guids(v).iter().map(|t| self.tags.name(t)).collect()
    }

    /// `optionalData` lists (V1 entries, V3 archetype entries, V3 table entries) as `[{kind, ..}]`.  Struct names and fields are CONFIRMED against
    /// the Game2.dcb schema; what the engine does with them is not (see "Amounts").
    fn optional(&self, v: &Value) -> Vec<Value> {
        let range = |q: &Value| json!([f(g(q, "min")), f(g(q, "max"))]);
        arr(v)
            .iter()
            .map(|o| {
                let st = s(g(o, "__struct__"));
                let kind = st.rsplit('_').next().unwrap_or(st);
                match st {
                    "EntryOptionalData_StackSize" => json!({"kind": "stack", "range": range(o)}),
                    "ArchetypeOptionalDataV3_StackSize" => json!({"kind": "stack", "range": range(g(o, "stackSize"))}),
                    "EntryOptionalData_SpawnWith" => json!({"kind": "spawn_with", "name": s(g(o, "name")), "mode": enum_str(g(o, "mode")), "range": range(o),
                        "tags": self.tag_names(g(g(o, "tagsToMatch"), "positiveTags")), "not": self.tag_names(g(g(o, "tagsToMatch"), "negativeTags")),
                        "pos": self.guids(g(g(o, "tagsToMatch"), "positiveTags")), "neg": self.guids(g(g(o, "tagsToMatch"), "negativeTags"))}),
                    "ArchetypeOptionalDataV3_SpawnWith" => {
                        let sel = g(o, "selector");
                        let mut names = vec![];
                        collect_refs(sel, &mut names);
                        let classes: Vec<&str> = names.iter().filter_map(|x| self.class_name(x)).collect();
                        json!({"kind": "spawn_with", "name": s(g(o, "name")), "chance": f(g(o, "chanceToSpawnWith")), "mode": enum_str(g(o, "mode")),
                               "range": range(g(o, "amountToSpawn")), "tags": self.tag_names(g(g(sel, "tags"), "positiveTags")),
                               "not": self.tag_names(g(g(sel, "tags"), "negativeTags")), "classes": classes.iter().take(20).collect::<Vec<_>>(),
                               "pos": self.guids(g(g(sel, "tags"), "positiveTags")), "neg": self.guids(g(g(sel, "tags"), "negativeTags")), "cls": classes.iter().take(400).collect::<Vec<_>>()})
                    }
                    "LootTableOptionalDataV3_ChanceToExist" => json!({"kind": "chance_to_exist", "value": f(g(o, "chanceToExist"))}),
                    "LootTableOptionalDataV3_ChoiceLimit" => json!({"kind": "choice_limit", "value": f(g(o, "choiceLimit"))}),
                    "LootTableOptionalDataV3_DupeLimit" => json!({"kind": "dupe_limit", "value": f(g(o, "dupeLimit"))}),
                    "LootTableOptionalDataV3_PriorityChoice" => json!({"kind": "priority", "value": f(g(o, "priorityCount"))}),
                    _ => json!({"kind": kind, "struct": st}),
                }
            })
            .collect()
    }

    /// Names of the `PoolFilterInstance`s of a pool filter, record references followed (4 levels).  What a filter removes from the pool is not
    /// interpreted (UNKNOWN): the names ("No faction", "No special events") are all the data says.
    fn filter_steps(&self, v: &Value, depth: usize, out: &mut Vec<String>) {
        if depth > 4 {
            return;
        }
        match v {
            Value::Array(a) => {
                if let Some(gd) = ref_guid(v) {
                    if let Some((_, rv)) = self.rec(gd, 12) {
                        self.filter_steps(g(&rv, "filter"), depth + 1, out);
                    }
                } else {
                    a.iter().for_each(|x| self.filter_steps(x, depth, out));
                }
            }
            Value::Object(o) => {
                if o.contains_key("name") && o.contains_key("filter") {
                    let n = s(g(v, "name"));
                    if !n.is_empty() && !out.iter().any(|x| x == n) {
                        out.push(n.to_string());
                    }
                }
                o.values().for_each(|x| self.filter_steps(x, depth, out));
            }
            _ => {}
        }
    }

    /// The pool filter as an expression for `filter_passes` (struct names and fields CONFIRMED against the Game2.dcb schema: `PoolFilter_Tags.
    /// tagFilter{positiveTags,negativeTags}`, `PoolFilter_RecordRef.filterRecord`, `PoolFilter_Sequence.filters[PoolFilterInstance{name, mode, filter}]`;
    /// `PoolFilter_EntityClasses` occurs in no record of this build and is exported as `unknown`).
    fn filter_expr(&self, v: &Value, depth: usize) -> Value {
        if depth > 6 {
            return json!({"t": "unknown"});
        }
        match s(g(v, "__struct__")) {
            "PoolFilter_Tags" => {
                let t = g(v, "tagFilter");
                json!({"t": "tags", "pos": self.guids(g(t, "positiveTags")), "neg": self.guids(g(t, "negativeTags"))})
            }
            "PoolFilter_RecordRef" => match ref_guid(g(v, "filterRecord")).and_then(|gd| self.rec(gd, 12)) {
                Some((_, rv)) => self.filter_expr(g(&rv, "filter"), depth + 1),
                None => json!({"t": "unknown"}),
            },
            "PoolFilter_Sequence" => json!({"t": "seq", "f": arr(g(v, "filters")).iter().map(|i| {
                json!({"or": enum_str(g(i, "mode")) != "Cumulative", "x": self.filter_expr(g(i, "filter"), depth + 1)})
            }).collect::<Vec<_>>()}),
            _ => json!({"t": "unknown"}),
        }
    }

    /// `LootConstraints` of a slot: everything that can change how many items a crate gets.
    fn constraints(&self, c: &Value) -> Value {
        let ff = g(c, "fullnessFactorRange");
        let mut j = json!({
            "fullness": [f(g(ff, "min")), f(g(ff, "max"))],
            "chance": if g(c, "chanceToGenerate").is_null() { 1.0 } else { f(g(c, "chanceToGenerate")) },
            "total_limit": f(g(c, "totalResultsLimit")),
            "extra_inv_chance": f(g(c, "chanceToGenerateAdditionalAttachedInventories")),
            "pruning": enum_str(g(g(c, "advanced"), "pruningLevel")),
            "fullness_mode": enum_str(g(g(c, "advanced"), "fullnessMode")),
        });
        if let Some((n, v)) = ref_guid(g(c, "poolFilter")).and_then(|gd| self.rec(gd, 12)) {
            let mut steps = vec![];
            self.filter_steps(g(&v, "filter"), 0, &mut steps);
            j["pool_filter"] = json!({"name": Reader::short(&n), "steps": steps, "expr": self.filter_expr(g(&v, "filter"), 0)});
        }
        let sc = g(c, "secondaryChoices");
        let (kind, rec) = match s(g(sc, "__struct__")) {
            "LootV3SecondaryChoicesRecordRef_MultiLayer" => ("multi", ref_guid(g(sc, "multiLayerRecord"))),
            "LootV3SecondaryChoicesRecordRef_SingleLayer" => ("single", ref_guid(g(sc, "singleLayerRecord"))),
            _ => ("", None),
        };
        if let Some((n, v)) = rec.and_then(|gd| self.rec(gd, 12)) {
            let layer = |lv: &Value| -> Value {
                let ch = arr(g(lv, "choices"));
                let tot: f64 = ch.iter().map(|x| f(g(x, "weight"))).sum();
                json!(ch.iter().map(|x| {
                    let tg = g(g(g(x, "selector"), "tags"), "positiveTags");
                    json!({"name": s(g(x, "name")), "weight": f(g(x, "weight")), "chance": if tot > 0.0 { f(g(x, "weight")) / tot } else { 0.0 }, "ok": g(g(x, "selector"), "tags").is_object(),
                           "tags": self.tag_names(tg), "not": self.tag_names(g(g(g(x, "selector"), "tags"), "negativeTags"))})
                }).collect::<Vec<_>>())
            };
            let layers: Vec<Value> = if kind == "multi" {
                self.guids(g(&v, "layers")).iter().filter_map(|gd| self.rec(gd, 12)).map(|(_, lv)| layer(&lv)).collect()
            } else {
                vec![layer(&v)]
            };
            j["secondary"] = json!({"name": Reader::short(&n), "layers": layers});
        }
        j
    }

    /// The loot config and fixed contents of an entity class (memoised).  `None` when the class record is unknown.
    fn class_fill(&self, cls: &str) -> Option<Rc<ClassFill>> {
        if let Some(c) = self.fills.borrow().get(cls) {
            return Some(c.clone());
        }
        let i = *self.d.record_index().get(&format!("{ENTITY}{cls}"))?.first()?;
        let rec = self.d.record(i).ok()?;
        // EMPIRICAL: depth 8 reads everything the loadout walker needs (same counts as 6) at a peak of 1.6 GB over the 29,693 entity classes;
        // depth 10 and 16 expand strong-pointer graphs of some classes (trolleys, UI panels) to 20+ GB.  Keep this low.
        let v = self.d.record_data(&rec, 8).ok()?.to_json();
        let c = Rc::new(lootclass::read(&v, self));
        self.fills.borrow_mut().insert(cls.to_string(), c.clone());
        Some(c)
    }

    fn class_name(&self, guid: &str) -> Option<&str> {
        let i = self.items.guid_record(guid)?;
        // borrowed from the record name table of the DataCore (lives as long as `self.d`)
        let n = self.d.record_name(i).ok()?;
        let n = n.strip_prefix(ENTITY)?.to_string();
        self.tags.class_ref(&n)
    }
}

fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(a) => {
            if let Some(g) = ref_guid(v) {
                out.push(g.to_string());
            } else {
                a.iter().for_each(|x| collect_refs(x, out));
            }
        }
        Value::Object(o) => o.values().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

/// How item tag sets are built (switches for surveying why pools come out empty).
#[derive(Clone, Copy, Debug)]
pub struct TagMode {
    /// an item also carries every ANCESTOR of its explicit tags (inverse of `Tag.children`).  CONFIRMED.
    /// With exact explicit membership 553 of the 1,716 entries of the 220 tables match nothing (e.g. the armor caches whose tag is the
    /// parent "Type")
    pub ancestors: bool,
    /// drop classes without `inventoryOccupancyVolume` (CONFIRMED)
    pub volume: bool,
    /// lootable admission (CONFIRMED): the lootable tag is the record with the FULL name `LootGeneration.CanGenerateAsLoot` (looked up by
    /// name when the item database is built; the record named `Lootable` is
    /// the wrong one: EMPIRICAL, it admits nothing).  A class is kept iff its explicit tags contain that tag or one of its descendants; the tag and
    /// its descendants are removed, the ancestors are recomputed from what remains, a class with nothing left is dropped (engine warning "has the
    /// lootable item tag, but no other tags!").  Skipped when the tag is not in the archive.
    pub admission: bool,
}

impl Default for TagMode {
    fn default() -> Self {
        TagMode { ancestors: true, volume: true, admission: true }
    }
}

/// `tag guid -> all its ancestors` from the `children` lists of the Tag records.
fn ancestors_of(children: &HashMap<String, Vec<String>>) -> HashMap<String, Vec<String>> {
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (p, cs) in children {
        for c in cs {
            parents.entry(c.as_str()).or_default().push(p.as_str());
        }
    }
    let mut out = HashMap::new();
    for t in parents.keys() {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack = vec![*t];
        while let Some(x) = stack.pop() {
            for p in parents.get(x).into_iter().flatten() {
                if seen.insert(p) {
                    stack.push(p);
                }
            }
        }
        out.insert(t.to_string(), seen.into_iter().map(String::from).collect());
    }
    out
}

/// Builds the tag tree and the item index from the DataCore.
pub fn read_tags(d: &Dcb) -> Tags {
    read_tags_mode(d, TagMode::default())
}

pub fn read_tags_mode(d: &Dcb, mode: TagMode) -> Tags {
    let mut names = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut raw: Vec<(String, Vec<String>)> = vec![];
    let mut excluded = 0usize;
    for i in 0..d.n_rec {
        let Ok(rec) = d.record(i) else { continue };
        match rec.struct_name.as_str() {
            "Tag" => {
                let Ok(v) = d.record_data(&rec, 1) else { continue };
                let v = v.to_json();
                names.insert(rec.guid.clone(), s(g(&v, "tagName")).to_string());
                let cs: Vec<String> = arr(g(&v, "children")).iter().filter_map(ref_guid).map(String::from).collect();
                if !cs.is_empty() {
                    children.insert(rec.guid.clone(), cs);
                }
            }
            "EntityClassDefinition" => {
                let Ok(v) = d.record_data(&rec, 1) else { continue };
                let v = v.to_json();
                let tags: Vec<String> = arr(g(&v, "tags")).iter().filter_map(ref_guid).map(String::from).collect();
                if tags.is_empty() {
                    continue;
                }
                // CONFIRMED: a lootable class without an inventory volume is not in the item database.
                // Volume = AttachDef.inventoryOccupancyVolume (micro-SCU); the field path is EMPIRICAL.
                let has_volume = d.record_data(&rec, 8).map(|full| {
                    let full = full.to_json();
                    let a = g(g(crate::itemstats::comp(&full, "SAttachableComponentParams"), "AttachDef"), "inventoryOccupancyVolume");
                    f(g(a, "microSCU")) > 0.0
                });
                if has_volume.unwrap_or(false) || !mode.volume {
                    raw.push((rec.name.strip_prefix(ENTITY).unwrap_or(&rec.name).to_string(), tags));
                } else {
                    excluded += 1;
                }
            }
            _ => {}
        }
    }
    let full = full_names(&names, &children);
    let loot_guids: Vec<String> = full.iter().filter(|(_, n)| n.as_str() == LOOT_TAG).map(|(gd, _)| gd.clone()).collect();
    let loot_tag_found = !loot_guids.is_empty();
    let before = raw.len();
    let raw: Vec<(String, Vec<String>)> = if mode.admission && loot_tag_found {
        let lootable = descendants_incl(&loot_guids, &children);
        raw.into_iter().filter_map(|(c, tags)| admit(&tags, &lootable).map(|t| (c, t))).collect()
    } else {
        raw
    };
    let excluded_not_lootable = before - raw.len();
    let items = if mode.ancestors {
        let anc = ancestors_of(&children);
        raw.into_iter()
            .map(|(c, mut tags)| {
                let mut seen: HashSet<String> = tags.iter().cloned().collect();
                for tg in tags.clone() {
                    for a in anc.get(&tg).into_iter().flatten() {
                        if seen.insert(a.clone()) {
                            tags.push(a.clone());
                        }
                    }
                }
                (c, tags)
            })
            .collect()
    } else {
        raw
    };
    let mut t = Tags::new(names, items);
    t.excluded_no_volume = excluded;
    t.excluded_not_lootable = excluded_not_lootable;
    t.loot_tag_found = loot_tag_found;
    t.full = full;
    t
}

/// Full name of the tag the engine tests for lootability (CONFIRMED: looked up by full name when the item database is built).
pub const LOOT_TAG: &str = "LootGeneration.CanGenerateAsLoot";

/// `guid -> Parent.Child` full names from the `children` lists (a tag with several parents climbs through the smallest parent guid; a cycle stops the climb).
pub fn full_names(names: &HashMap<String, String>, children: &HashMap<String, Vec<String>>) -> HashMap<String, String> {
    let mut parent: HashMap<&str, &str> = HashMap::new();
    for (p, cs) in children {
        for c in cs {
            let e = parent.entry(c.as_str()).or_insert(p.as_str());
            if p.as_str() < *e {
                *e = p.as_str();
            }
        }
    }
    let mut out = HashMap::new();
    for (gd, n) in names {
        let mut parts = vec![n.as_str()];
        let mut cur = gd.as_str();
        let mut seen: HashSet<&str> = HashSet::from([cur]);
        while let Some(&p) = parent.get(cur) {
            if !seen.insert(p) {
                break;
            }
            parts.push(names.get(p).map_or(p, |x| x.as_str()));
            cur = p;
        }
        parts.reverse();
        out.insert(gd.clone(), parts.join("."));
    }
    out
}

/// The given tags and everything below them.
pub fn descendants_incl(roots: &[String], children: &HashMap<String, Vec<String>>) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut stack: Vec<String> = roots.to_vec();
    while let Some(x) = stack.pop() {
        if out.insert(x.clone()) {
            stack.extend(children.get(&x).cloned().unwrap_or_default());
        }
    }
    out
}

/// The admission rule (CONFIRMED): `lootable` = the lootable tag and its descendants.  Kept iff a class tag is in that set
/// (the tag itself or any child of it); the set's members are removed; None when nothing remains.
pub fn admit(tags: &[String], lootable: &HashSet<String>) -> Option<Vec<String>> {
    if !tags.iter().any(|t| lootable.contains(t)) {
        return None;
    }
    let rest: Vec<String> = tags.iter().filter(|t| !lootable.contains(*t)).cloned().collect();
    (!rest.is_empty()).then_some(rest)
}

/// `Q : w,x,y,z | T : x,y,z` -> position.
pub fn parse_transform(t: &str) -> Option<[f64; 3]> {
    let tp = t.split("T :").nth(1)?;
    let v: Vec<f64> = tp.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (v.len() == 3).then(|| [v[0], v[1], v[2]])
}

/// `Q : w,x,y,z | T : ...` -> the quaternion (w,x,y,z).  EMPIRICAL: the attribute carries it first, the same order as the entity `Rotate`.
pub fn parse_rotation(t: &str) -> Option<[f64; 4]> {
    let qp = t.split("T :").next()?.split("Q :").nth(1)?;
    let v: Vec<f64> = qp.trim().trim_end_matches('|').split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
}

/// A spawn point as read from a container: position, tag guids, orientation when the source has one (w,x,y,z).
pub type SpawnPt = ([f64; 3], Vec<String>, Option<[f64; 4]>);

/// Spawn points of a child container: `(position, tag guids)`.
pub fn spawn_points(xml: &Element) -> Vec<([f64; 3], Vec<String>)> {
    spawn_points_rot(xml).into_iter().map(|(p, t, _)| (p, t)).collect()
}

/// [`spawn_points`] with the orientation of each point (used to turn the crate models on the 3D map).
pub fn spawn_points_rot(xml: &Element) -> Vec<SpawnPt> {
    let mut out = vec![];
    for e in iter_all(xml).into_iter().filter(|e| e.tag == "Entity") {
        let Some(t) = e.get("relativeTransform") else { continue };
        let Some(p) = parse_transform(t) else { continue };
        let tags = e.get("tags").unwrap_or("").split(',').filter(|t| !t.is_empty()).map(String::from).collect();
        out.push((p, tags, parse_rotation(t)));
    }
    out
}

/// One harvestable entity found in a container.
pub struct Found {
    pub container: String,
    pub name: String,
    pub cls: String,
    pub pos: [f64; 3],
    /// `Rotate` (w,x,y,z) and `Scale` of the entity: the transform that places the child container in this container
    /// (same convention as `socpak::entity_from_root`, which builds the nested-container matrix the zone compiler uses).
    pub rot: [f64; 4],
    pub scale: [f64; 3],
    pub config: String,
    pub child: Option<String>,
    /// row-major 3x4 placement of the child container as the owning container's metadata XML declares it (`ChildObjectContainers` `pos` /
    /// `rot`, the matrix the zone compiler uses); set by `scan_containers`.  EMPIRICAL: securitypostkareah_int declares its loot child at
    /// (27.3, 2.7, -3.0) while the entity's own `Pos` is (140.7, 1385.0, -20.7); the declared one puts the points on the building.
    pub child_matrix: Option<[f64; 12]>,
}

/// `n` comma separated floats, or None when the count differs.
fn floats<const N: usize>(s: Option<&str>) -> Option<[f64; N]> {
    let v: Vec<f64> = s?.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    <[f64; N]>::try_from(v).ok()
}

/// The harvestable-config entities of one parsed `.entxml` (`HarvestableComponent.looseSubConfigBase`).
pub fn harvestable_entity(container: &str, root: &Element) -> Option<Found> {
    let all = iter_all(root);
    let base = all.iter().find(|e| e.tag == "looseSubConfigBase")?;
    let config = base.get("multiConfigRef").or_else(|| base.get("subConfigRef")).filter(|g| *g != NULL_GUID && !g.is_empty())?;
    let child = all.iter().find(|e| e.tag == "EntityComponentObjectContainer").and_then(|e| e.get("objectContainer")).filter(|c| !c.is_empty());
    let pos: Vec<f64> = root.get("Pos").unwrap_or("").split(',').filter_map(|x| x.trim().parse().ok()).collect();
    Some(Found {
        container: container.to_string(),
        name: root.get("Name").unwrap_or("").to_string(),
        cls: root.get("EntityClass").unwrap_or("").to_string(),
        pos: if pos.len() == 3 { [pos[0], pos[1], pos[2]] } else { [0.0; 3] },
        rot: floats(root.get("Rotate")).unwrap_or([1.0, 0.0, 0.0, 0.0]),
        scale: floats(root.get("Scale")).unwrap_or([1.0; 3]),
        config: config.to_string(),
        child: child.map(String::from),
        child_matrix: None,
    })
}

/// A sub config's slots: `(initialSlotsProbability, [slot])`.
fn slots_of(r: &Reader, cfg: &Value) -> (f64, Vec<Value>) {
    let init = if cfg.get("initialSlotsProbability").is_some_and(|v| !v.is_null()) { f(g(cfg, "initialSlotsProbability")) } else { 1.0 };
    let ss = arr(g(cfg, "subHarvestables"));
    let total: f64 = ss.iter().map(|x| f(g(x, "relativeProbability"))).sum();
    let mut out = vec![];
    for sl in ss {
        let preset = ref_guid(g(sl, "harvestable")).and_then(|gd| r.rname(gd)).map(|n| Reader::short(&n).to_string());
        // the crate entity: given directly, else the entity class of the preset (EMPIRICAL: `HarvestablePreset.entityClass`)
        let ent = ref_guid(g(sl, "harvestableEntityClass"))
            .and_then(|gd| r.rname(gd))
            .or_else(|| ref_guid(g(sl, "harvestable")).and_then(|gd| r.rec(gd, 1)).and_then(|(_, v)| ref_guid(g(&v, "entityClass")).and_then(|c| r.rname(c))))
            .map(|n| Reader::short(&n).to_string());
        let lc = g(sl, "lootConfig");
        let mut c = g(lc, "lootConstraints").clone();
        let t1 = ref_guid(g(lc, "lootTable")).and_then(|gd| r.rname(gd));
        let t3 = ref_guid(g(lc, "lootTableV3")).and_then(|gd| r.rname(gd));
        let mut table = t3.or(t1);
        // CONFIRMED (see `lootclass`): without a table in the slot's lootConfig the entity is still spawned and filled from its own class
        let fill = ent.as_deref().and_then(|e| r.class_fill(e));
        let mut src = if table.is_some() { Some("slot") } else { None };
        if let (None, Some(cl)) = (&table, fill.as_deref().and_then(|x| x.loot.as_ref())) {
            table = cl.table_v3.clone().or_else(|| cl.table.clone());
            c = cl.constraints.clone();
            src = Some("class");
        }
        let cons = r.constraints(&c);
        let fixed = fill.as_deref().map(lootclass::to_json);
        let has_fixed = fill.as_deref().is_some_and(|x| !x.fixed.is_empty());
        out.push(json!({
            "preset": preset, "entity": ent,
            "share": if total > 0.0 { f(g(sl, "relativeProbability")) / total } else { 0.0 },
            "table": table, "table_src": src,
            "fullness": [f(g(g(&c, "fullnessFactorRange"), "min")), f(g(g(&c, "fullnessFactorRange"), "max"))],
            "chance": if truthy(g(&c, "chanceToGenerate")) { f(g(&c, "chanceToGenerate")) } else { 0.0 },
            "cons": cons,
            "respawn_mult": f(g(sl, "harvestableRespawnTimeMultiplier")),
            "class": fixed,
            // what the slot gives: a loot table (slot or class), fixed loadout items, both, or only the item itself
            "fill": match (table.is_some(), has_fixed) { (true, true) => "table+fixed", (true, false) => "table", (false, true) => "fixed", _ => "item" },
        }));
    }
    (init, out)
}

/// Groups of a config record: `[{name, tags: [guid], init, slots}]`; the default config has an empty `tags` and `default: true`.
fn config_groups(r: &Reader, rec: &Value) -> Vec<Value> {
    let mut groups = vec![];
    let sub = |r: &Reader, sc: &Value| -> Option<(f64, Vec<Value>)> {
        match s(g(sc, "__struct__")) {
            "SubHarvestableConfigSingleManual" => Some(slots_of(r, g(sc, "subConfigManual"))),
            "SubHarvestableConfigSingleRef" => ref_guid(g(sc, "subConfigRef")).and_then(|gd| r.rec(gd, 12)).map(|(_, v)| slots_of(r, g(&v, "subConfig"))),
            _ => None,
        }
    };
    if rec.get("subConfig").is_some() {
        let (init, slots) = slots_of(r, g(rec, "subConfig"));
        groups.push(json!({"name": "all", "tags": [], "default": true, "init": init, "slots": slots}));
        return groups;
    }
    let mc = g(rec, "multiConfig");
    for t in arr(g(mc, "taggedConfigs")) {
        let tags = r.guids(g(g(t, "tagList"), "tags"));
        if let Some((init, slots)) = sub(r, g(t, "subConfig")) {
            groups.push(json!({"name": s(g(t, "name")), "tags": tags, "default": false, "init": init, "slots": slots}));
        }
    }
    if let Some((init, slots)) = sub(r, g(mc, "defaultConfig")) {
        groups.push(json!({"name": "default", "tags": [], "default": true, "init": init, "slots": slots}));
    }
    groups
}

/// Adds the selection controls of each table archetype: `prio` (priorityCount), `limit` (choiceLimit, 0 = none), `cte` (chance to exist),
/// `dupe` (dupeLimit, 0 = none) and `eff_chance` (weight x cte, renormalised over the table).  CONFIRMED: V3 `PriorityChoice(priorityCount)`
/// and V1 `minResults` both become priorityCount, V1 `maxResults` becomes choiceLimit (0 = none); `ChanceToExist` is rolled per entry
/// at tree build and weights are NOT renormalised after a failure, so the entry's effective weight is about `weight x chance`.
fn post_process_arch(arch: &mut [Value], v1: bool) {
    let mut effs = vec![];
    for a in arch.iter_mut() {
        let opt = |k: &str| -> f64 { arr(g(a, "opt")).iter().find(|o| o["kind"] == k).map_or(0.0, |o| f(g(o, "value"))) };
        let has = |k: &str| arr(g(a, "opt")).iter().any(|o| o["kind"] == k);
        let (prio, limit) = if v1 { (f(g(a, "min")), f(g(a, "max"))) } else { (opt("priority"), opt("choice_limit")) };
        let cte = if has("chance_to_exist") { opt("chance_to_exist") } else { 1.0 };
        let dupe = opt("dupe_limit");
        effs.push(f(g(a, "weight")) * cte);
        a["prio"] = json!(prio.max(0.0));
        a["limit"] = json!(limit.max(0.0));
        a["cte"] = json!(cte);
        a["dupe"] = json!(dupe);
    }
    let tot: f64 = effs.iter().sum();
    for (a, e) in arch.iter_mut().zip(effs) {
        a["eff_chance"] = json!(if tot > 0.0 { e / tot } else { 0.0 });
    }
}

/// All loot tables, with their archetypes and pools: `{tables: {name: {kind, arch: [..]}}}`.
fn tables(r: &Reader, wanted: &HashSet<String>) -> Value {
    let mut out = Map::new();
    let mut memo: HashMap<String, Value> = HashMap::new();
    for name in wanted {
        let Some(&i) = r.d.record_index().get(name).and_then(|v| v.first()) else { continue };
        let Ok(rec) = r.d.record(i) else { continue };
        let Ok(v) = r.d.record_data(&rec, 12) else { continue };
        let v = v.to_json();
        let mut arch = vec![];
        if rec.struct_name == "LootTable" {
            let es = arr(g(&v, "lootArchetypes"));
            let total: f64 = es.iter().map(|e| f(g(e, "weight"))).sum();
            for e in es {
                let Some((an, av)) = ref_guid(g(e, "archetype")).and_then(|gd| r.rec(gd, 12)) else { continue };
                let body = memo.entry(an.clone()).or_insert_with(|| r.archetype_v1(&av)).clone();
                let mn = g(g(e, "numberOfResultsConstraints"), "minResults");
                let mx = g(g(e, "numberOfResultsConstraints"), "maxResults");
                arch.push(json!({"name": Reader::short(&an), "weight": f(g(e, "weight")), "chance": if total > 0.0 { f(g(e, "weight")) / total } else { 0.0 },
                                 "min": mn, "max": mx, "body": body}));
            }
        } else if rec.struct_name == "LootTableV3Record" {
            let es = arr(g(g(&v, "lootTable"), "lootArchetypes"));
            let total: f64 = es.iter().map(|e| f(g(e, "weight"))).sum();
            for e in es {
                let a = g(e, "archetype");
                let (an, body) = if s(g(a, "__struct__")) == "LootArchetypeV3_RecordRef" {
                    let Some((an, av)) = ref_guid(g(a, "lootArchetypeRecord")).and_then(|gd| r.rec(gd, 12)) else { continue };
                    let b = memo.entry(an.clone()).or_insert_with(|| r.archetype_v3(g(&av, "lootArchetype"))).clone();
                    (Reader::short(&an).to_string(), b)
                } else {
                    (s(g(e, "name")).to_string(), r.archetype_v3(a))
                };
                arch.push(json!({"name": an, "label": s(g(e, "name")), "weight": f(g(e, "weight")), "chance": if total > 0.0 { f(g(e, "weight")) / total } else { 0.0 }, "opt": r.optional(g(e, "optionalData")), "body": body}));
            }
        }
        post_process_arch(&mut arch, rec.struct_name == "LootTable");
        out.insert(name.clone(), json!({"id": Reader::short(name), "kind": if rec.struct_name == "LootTable" { "v1" } else { "v3" }, "arch": arch}));
    }
    Value::Object(out)
}

/// Resolves a child container path of an entity (`ObjectContainers/...socpak`, any case) to its archive entry name.
pub fn resolve_container(p4k: &P4k, path: &str) -> Option<String> {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches('/');
    let cand = if p.to_lowercase().starts_with("data/") { p.to_string() } else { format!("Data/{}", p) };
    if p4k.get(&cand).is_some() {
        return Some(cand);
    }
    let idx = p4k.lower_index(".socpak");
    idx.map.get(&cand.to_lowercase()).map(|&i| p4k.entries[i].name.to_string())
}

/// Spawn points of the static entities of a `.soc`: `HarvestableTagPoint` entities (`Pos`, `Tags/Tag@TagId`).  EMPIRICAL: older
/// containers (derelicts, stations) place their points this way, newer ones through `entitycomponentslotspawnpoint.xml`.
pub fn soc_tag_points(root: &Element) -> Vec<([f64; 3], Vec<String>)> {
    soc_tag_points_rot(root).into_iter().map(|(p, t, _)| (p, t)).collect()
}

/// [`soc_tag_points`] with the entity `Rotate` (w,x,y,z) when present.
pub fn soc_tag_points_rot(root: &Element) -> Vec<SpawnPt> {
    let mut out = vec![];
    for e in iter_all(root).into_iter().filter(|e| e.tag == "Entity" && e.get("EntityClass") == Some("HarvestableTagPoint")) {
        let pos: Vec<f64> = e.get("Pos").unwrap_or("").split(',').filter_map(|x| x.trim().parse().ok()).collect();
        if pos.len() != 3 {
            continue;
        }
        let mut tags = vec![];
        if let Some(t) = e.find("Tags") {
            tags.extend(t.children.iter().filter(|c| c.tag == "Tag").filter_map(|c| c.get("TagId")).map(String::from));
        }
        out.push(([pos[0], pos[1], pos[2]], tags, floats::<4>(e.get("Rotate"))));
    }
    out
}

/// All spawn points of a child container (both sources).
fn read_points(p4k: &P4k, entry: &str) -> Vec<SpawnPt> {
    let Some(e) = p4k.get(entry) else { return vec![] };
    let Ok(data) = p4k.read(e) else { return vec![] };
    let Ok(z) = SocZip::open(&data) else { return vec![] };
    let names: Vec<String> = z.namelist().map(String::from).collect();
    let mut out = vec![];
    if let Some(n) = names.iter().find(|n| n.to_lowercase().ends_with("entitycomponentslotspawnpoint.xml")) {
        if let Some(x) = z.read(n).ok().and_then(|b| cryxml::parse(&b).ok()) {
            out.extend(spawn_points_rot(&x));
        }
    }
    for n in names.iter().filter(|n| n.to_lowercase().ends_with(".soc")) {
        let Ok(b) = z.read(n) else { continue };
        let Some(o) = b.windows(7).position(|w| w == b"CryXmlB") else { continue };
        if let Ok(x) = cryxml::parse(&b[o..]) {
            out.extend(soc_tag_points_rot(&x));
        }
    }
    out
}

/// The metadata placement of the child that belongs to `f`: same entity name, else the same child container path (only when unique).
fn child_matrix(kids: &[p4k_formats::socpak::Entity], f: &Found) -> Option<[f64; 12]> {
    if let Some(k) = kids.iter().find(|k| !f.name.is_empty() && k.name == f.name) {
        return Some(k.matrix);
    }
    let key = container_key(f.child.as_deref()?);
    let mut same = kids.iter().filter(|k| k.container.as_deref().map(container_key).as_deref() == Some(key.as_str()));
    let first = same.next()?;
    if same.next().is_none() {
        Some(first.matrix)
    } else {
        None
    }
}

/// Scans every container for harvestable entities; returns them with the container path.
pub fn scan_containers(p4k: &P4k) -> Vec<Found> {
    let mut out = vec![];
    for e in p4k.entries.iter().filter(|e| e.name.to_lowercase().ends_with(".socpak") && e.name.contains("ObjectContainers")) {
        let Ok(data) = p4k.read(e) else { continue };
        let Ok(z) = SocZip::open(&data) else { continue };
        let names: Vec<String> = z.namelist().filter(|n| n.ends_with(".entxml") && n.contains("entdata")).map(String::from).collect();
        let mut kids: Option<Vec<p4k_formats::socpak::Entity>> = None;
        for n in names {
            let Ok(b) = z.read(&n) else { continue };
            if !memchr::memmem::find(&b, b"HarvestableComponent").is_some() {
                continue;
            }
            let Ok(root) = cryxml::parse(&b) else { continue };
            if let Some(mut f) = harvestable_entity(&e.name, &root) {
                let kids = kids.get_or_insert_with(|| p4k_formats::socpak::read_child_containers(&z));
                f.child_matrix = child_matrix(kids, &f);
                out.push(f);
            }
        }
    }
    out
}

/// Key of a container path for the reverse lookup: lower case, forward slashes, without the leading `Data/`.
pub fn container_key(p: &str) -> String {
    let p = p.replace('\\', "/").to_lowercase();
    p.trim_start_matches('/').trim_start_matches("data/").to_string()
}

/// A container that lists another one as a child (`ChildObjectContainers`).
#[derive(Debug, Clone, PartialEq)]
pub struct Parent {
    /// archive entry name of the parent container
    pub path: String,
    /// row-major 3x4 placement of the child inside the parent (`socpak::read_child_containers`: rotation w,x,y,z, no scale)
    pub matrix: [f64; 12],
}

/// Reverse child-container lookup: for every container of the archive, the children it declares in its metadata XML, kept only for the
/// `targets` (keys from [`container_key`]).  Many `ugf/.../loot` overlay containers have no geometry of their own: the parent structure that
/// lists them as a child has it.  CONFIRMED in code: the zone compiler places a child with exactly this matrix (`read_child_containers`).
/// Reads every container once (seconds), so callers cache the result.
pub fn parents_of(p4k: &P4k, targets: &HashSet<String>) -> HashMap<String, Vec<Parent>> {
    let mut out: HashMap<String, Vec<Parent>> = HashMap::new();
    for e in p4k.entries.iter().filter(|e| e.name.to_lowercase().ends_with(".socpak") && e.name.contains("ObjectContainers")) {
        let Ok(data) = p4k.read(e) else { continue };
        let Ok(z) = SocZip::open(&data) else { continue };
        for c in p4k_formats::socpak::read_child_containers(&z) {
            let Some(path) = c.container.as_deref() else { continue };
            let key = container_key(path);
            if targets.contains(&key) {
                out.entry(key).or_default().push(Parent { path: e.name.to_string(), matrix: c.matrix });
            }
        }
    }
    out
}

/// The whole index: `{v, locations, tables, stats}`.
pub fn build(items: &ItemMaterials, p4k: &P4k) -> Value {
    build_mode(items, p4k, TagMode::default())
}

pub fn build_mode(items: &ItemMaterials, p4k: &P4k, mode: TagMode) -> Value {
    let Some(d) = items.datacore() else { return json!({"v": 1, "locations": [], "tables": {}, "stats": {}}) };
    let r = Reader { items, d, tags: read_tags_mode(d, mode), fills: RefCell::default() };
    let found = scan_containers(p4k);
    let mut cfg_cache: HashMap<String, Option<(String, Vec<Value>)>> = HashMap::new();
    let mut spawn_cache: HashMap<String, Vec<SpawnPt>> = HashMap::new();
    let mut by_loc: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut used_tables: HashSet<String> = HashSet::new();
    let (mut pts_total, mut pts_changed, mut pts_unmatched) = (0usize, 0usize, 0usize);
    let (mut no_loot, mut no_points) = (0usize, 0usize);
    // survey of what the slots give (distinct config records; `placed` = expected filled spawn points, init x share x points of the kept entities)
    let mut survey: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut surveyed: HashSet<String> = HashSet::new();
    for fe in &found {
        let cfg = cfg_cache.entry(fe.config.clone()).or_insert_with(|| r.rec(&fe.config, 12).map(|(n, v)| (n, config_groups(&r, &v)))).clone();
        let Some((cname, groups)) = cfg else { continue };
        if surveyed.insert(fe.config.clone()) {
            for gr in &groups {
                for sl in arr(g(gr, "slots")) {
                    let key = format!("{}|{}", s(g(sl, "fill")), s(g(sl, "table_src")));
                    survey.entry(key).or_default().0 += 1;
                }
            }
        }
        // a slot is useful when it gives a loot table (its own or its class's) or fixed loadout items; item-only slots place nothing to look up
        let has_loot = groups.iter().any(|gr| arr(g(gr, "slots")).iter().any(|sl| s(g(sl, "fill")) != "item"));
        if !has_loot {
            no_loot += 1;
            continue;
        }
        // the archive name of the child (real case: /zone/ and /zonedata/ paths are case-sensitive, the entity string is not always cased like it)
        let child_real = fe.child.as_ref().and_then(|c| resolve_container(p4k, c));
        let pts = child_real.clone().map(|c| {
            spawn_cache
                .entry(c.clone())
                .or_insert_with(|| read_points(p4k, &c))
                .clone()
        });
        let pts = pts.unwrap_or_default();
        if pts.is_empty() {
            no_points += 1;
        }
        // CONFIRMED matching (`pick_group`): most specific tagged config first, all of its tags among the point's (names), else default
        let gtags: Vec<(bool, Vec<String>)> = groups
            .iter()
            .map(|gr| (gr["default"].as_bool().unwrap_or(false), arr(g(gr, "tags")).iter().map(|t| r.tags.full_name(s(t))).collect()))
            .collect();
        let mut counts = vec![0usize; groups.len()];
        let mut points = vec![];
        for (p, tags, rot) in &pts {
            let names = point_tag_names(&r.tags, tags);
            let gi = pick_group(&gtags, &names);
            // measurement of the wrong earlier rule (markers ADDED, short names), kept as a stat for comparison
            let mut old: HashSet<String> = tags.iter().map(|t| r.tags.name(t)).collect();
            old.extend(POINT_ENGINE_TAGS.iter().map(|x| x.to_string()));
            let old_gtags: Vec<(bool, Vec<String>)> = groups
                .iter()
                .map(|gr| (gr["default"].as_bool().unwrap_or(false), arr(g(gr, "tags")).iter().map(|t| r.tags.name(s(t))).collect()))
                .collect();
            let go = pick_group(&old_gtags, &old);
            pts_total += 1;
            pts_changed += (go != gi) as usize;
            pts_unmatched += gi.is_none() as usize;
            if let Some(gi) = gi {
                counts[gi] += 1;
            }
            let mut pj = vec![json!(round1(p[0])), json!(round1(p[1])), json!(round1(p[2])), json!(gi.map_or(-1, |x| x as i64))];
            if let Some(q) = rot {
                pj.push(json!(q.iter().map(|v| (v * 1e4).round() / 1e4).collect::<Vec<f64>>()));
            }
            points.push(Value::Array(pj));
        }
        let mut gout = vec![];
        for (gi, gr) in groups.iter().enumerate() {
            for sl in arr(g(gr, "slots")) {
                if let Some(t) = sl["table"].as_str() {
                    used_tables.insert(t.to_string());
                }
            }
            for sl in arr(g(gr, "slots")) {
                let key = format!("{}|{}", s(g(sl, "fill")), s(g(sl, "table_src")));
                survey.entry(key).or_default().1 += counts[gi] as f64 * f(g(gr, "init")) * f(g(sl, "share"));
            }
            let tn: Vec<String> = arr(g(gr, "tags")).iter().map(|t| r.tags.name(s(t))).collect();
            gout.push(json!({"name": gr["name"], "tags": tn, "default": gr["default"], "init": gr["init"], "slots": gr["slots"], "points": counts[gi]}));
        }
        by_loc.entry(fe.container.clone()).or_default().push(json!({
            "name": fe.name, "cls": fe.cls, "pos": fe.pos, "rot": fe.rot, "scale": fe.scale, "cm": fe.child_matrix, "config": Reader::short(&cname), "child": fe.child, "child_path": child_real, "groups": gout, "points": points,
        }));
    }
    let tables = tables(&r, &used_tables);
    let locations: Vec<Value> = by_loc
        .into_iter()
        .enumerate()
        .map(|(id, (path, ents))| {
            let name = path.rsplit('/').next().unwrap_or(&path).trim_end_matches(".socpak").to_string();
            let dir = path.rsplit_once('/').map_or("", |x| x.0).trim_start_matches("Data/ObjectContainers/").to_string();
            let npts: usize = ents.iter().map(|e| arr(g(e, "points")).len()).sum();
            json!({"id": id, "name": name, "dir": dir, "path": path, "entities": ents, "points": npts})
        })
        .collect();
    // class level survey: the distinct entity classes of the slots of the kept configs
    let fills = r.fills.borrow();
    let mut unresolved: BTreeMap<String, usize> = BTreeMap::new();
    for c in fills.values() {
        for u in &c.unresolved {
            *unresolved.entry(u.split(':').next().unwrap_or(u).to_string()).or_default() += 1;
        }
    }
    let class_survey = json!({"classes": fills.len(), "with_class_loot": fills.values().filter(|c| c.loot.is_some()).count(),
        "with_fixed": fills.values().filter(|c| !c.fixed.is_empty()).count(), "with_unresolved_loadout": fills.values().filter(|c| !c.unresolved.is_empty()).count(),
        "unresolved_kinds": unresolved, "with_inventory": fills.values().filter(|c| c.inventory).count()});
    let (mut entries_total, mut entries_empty) = (0usize, 0usize);
    for tb in tables.as_object().into_iter().flat_map(|o| o.values()) {
        for ar in arr(g(tb, "arch")) {
            for en in arr(g(g(ar, "body"), "entries")) {
                entries_total += 1;
                entries_empty += (g(en, "n").as_u64() == Some(0)) as usize;
            }
        }
    }
    let stats = json!({"class_survey": class_survey, "pool_entries": entries_total, "pool_entries_empty": entries_empty, "harvestable_entities": found.len(), "without_loot_table": no_loot, "without_spawn_points": no_points, "locations": locations.len(),
                       "tables": used_tables.len(),
                       "slot_survey": survey.iter().map(|(k, v)| (k.clone(), json!({"slots": v.0, "placed": (v.1 * 100.0).round() / 100.0}))).collect::<Map<String, Value>>(),
                       "items_indexed": r.tags.n_items(), "items_excluded_no_volume": r.tags.excluded_no_volume,
                       "items_excluded_not_lootable": r.tags.excluded_not_lootable, "loot_tag_found": r.tags.loot_tag_found,
                       "points": pts_total, "points_group_changed_by_marker_rule": pts_changed, "points_unmatched": pts_unmatched});
    json!({"v": 1, "locations": locations, "tables": tables, "stats": stats})
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags() -> Tags {
        let names: HashMap<String, String> = [("a", "Armor"), ("b", "Helmet"), ("c", "Common"), ("d", "Rare")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Tags::new(
            names,
            vec![
                ("helm_c".into(), vec!["b".into(), "c".into()]),
                ("helm_r".into(), vec!["b".into(), "d".into()]),
                ("arms_c".into(), vec!["a".into(), "c".into()]),
                ("gun".into(), vec!["c".into()]),
            ],
        )
    }

    #[test]
    fn floats_need_the_exact_count() {
        assert_eq!(floats::<3>(Some("1, 2.5,-3")), Some([1.0, 2.5, -3.0]));
        assert_eq!(floats::<4>(Some("1,2,3")), None);
        assert_eq!(floats::<3>(None), None);
    }

    #[test]
    fn pool_uses_exact_membership_and_negatives() {
        let t = tags();
        // exact membership in the (already ancestor-expanded) set given to Tags::new: here no item lists Armor for helm_*, so the Armor pool is just arms_c
        assert_eq!(t.pool(&["a".into()], &[]), vec!["arms_c"]);
        assert_eq!(t.pool(&["b".into()], &[]), vec!["helm_c", "helm_r"]);
        assert_eq!(t.pool(&["b".into(), "c".into()], &[]), vec!["helm_c"]);
        assert_eq!(t.pool(&["c".into()], &["a".into()]), vec!["gun", "helm_c"]);
        assert!(t.pool(&[], &[]).is_empty());
        assert!(t.pool(&["zz".into()], &[]).is_empty());
        assert_eq!(t.name("a"), "Armor");
        assert!(t.item_has_tag_like("helm_r", "rare") && t.item_has_tag_like("helm_r", "helmet") && !t.item_has_tag_like("helm_c", "rare"));
        assert!(!t.item_has_tag_like("nope", "rare"));
        assert_eq!(t.name("q"), "q");
    }

    #[test]
    fn container_keys_ignore_case_and_data_prefix() {
        assert_eq!(container_key("Data\\ObjectContainers\\A\\B.socpak"), "objectcontainers/a/b.socpak");
        assert_eq!(container_key("objectcontainers/a/b.socpak"), "objectcontainers/a/b.socpak");
    }

    #[test]
    fn transform_and_spawn_points() {
        assert_eq!(parse_transform("Q : 0.7,0,0,0.7 | T : 8.4,12.4,1.0"), Some([8.4, 12.4, 1.0]));
        assert_eq!(parse_transform("garbage"), None);
        let mut e = Element::default();
        e.tag = "Entity".into();
        e.attrs = vec![("relativeTransform".into(), "Q : 1,0,0,0 | T : 1,2,3".into()), ("tags".into(), "x,y".into())];
        let mut root = Element::default();
        root.tag = "EntityComponentSlotSpawnPoint".into();
        root.children.push(e);
        assert_eq!(spawn_points(&root), vec![([1.0, 2.0, 3.0], vec!["x".to_string(), "y".to_string()])]);
        assert_eq!(spawn_points_rot(&root)[0].2, Some([1.0, 0.0, 0.0, 0.0]));
        assert_eq!(parse_rotation("Q : 0.7,0,0,0.7 | T : 8.4,12.4,1.0"), Some([0.7, 0.0, 0.0, 0.7]));
        assert_eq!(parse_rotation("T : 1,2,3"), None);
    }

    #[test]
    fn soc_points_read_tag_points_only() {
        let tag = |id: &str| Element { tag: "Tag".into(), attrs: vec![("TagId".into(), id.into())], ..Default::default() };
        let tags = Element { tag: "Tags".into(), children: vec![tag("t1"), tag("t2")], ..Default::default() };
        let pt = Element { tag: "Entity".into(), attrs: vec![("EntityClass".into(), "HarvestableTagPoint".into()), ("Pos".into(), "1,2,3".into())], children: vec![tags], ..Default::default() };
        let other = Element { tag: "Entity".into(), attrs: vec![("EntityClass".into(), "Light".into()), ("Pos".into(), "9,9,9".into())], ..Default::default() };
        let root = Element { tag: "Entities".into(), children: vec![pt, other], ..Default::default() };
        assert_eq!(soc_tag_points(&root), vec![([1.0, 2.0, 3.0], vec!["t1".to_string(), "t2".to_string()])]);
    }

    #[test]
    fn harvestable_entity_needs_a_config() {
        let mk = |tag: &str, attrs: Vec<(&str, &str)>, kids: Vec<Element>| Element {
            tag: tag.into(),
            attrs: attrs.into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            text: String::new(),
            children: kids,
        };
        let base = mk("looseSubConfigBase", vec![("multiConfigRef", "c41d5ac6-d30d-4423-b074-57b5f7231447")], vec![]);
        let hc = mk("HarvestableComponent", vec![], vec![base]);
        let oc = mk("EntityComponentObjectContainer", vec![("objectContainer", "ObjectContainers/x/y.socpak")], vec![]);
        let props = mk("PropertiesDataCore", vec![], vec![oc, hc]);
        let root = mk("Entity", vec![("Name", "lootable_0001"), ("Pos", "13,6,240"), ("EntityClass", "HarvestableStreamableObjectContainer")], vec![props]);
        let f = harvestable_entity("Data/ObjectContainers/a.socpak", &root).unwrap();
        assert_eq!((f.name.as_str(), f.pos, f.child.as_deref()), ("lootable_0001", [13.0, 6.0, 240.0], Some("ObjectContainers/x/y.socpak")));
        let null = mk("looseSubConfigBase", vec![("subConfigRef", NULL_GUID)], vec![]);
        let root2 = mk("Entity", vec![], vec![mk("HarvestableComponent", vec![], vec![null])]);
        assert!(harvestable_entity("c", &root2).is_none());
    }

    #[test]
    fn ref_guid_skips_null_refs() {
        assert_eq!(ref_guid(&json!(["ref", 3, "abc"])), Some("abc"));
        assert_eq!(ref_guid(&json!(["ref", 4294967295u64, NULL_GUID])), None);
        assert_eq!(ref_guid(&json!("abc")), None);
    }

    fn tree() -> (HashMap<String, String>, HashMap<String, Vec<String>>) {
        let names: HashMap<String, String> =
            [("r", "LootGeneration"), ("l", "CanGenerateAsLoot"), ("lc", "Special"), ("x", "CanGenerateAsLoot"), ("o", "Lootable"), ("t", "Type"), ("h", "Helmet")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let ch: HashMap<String, Vec<String>> = [("r", vec!["l"]), ("l", vec!["lc"]), ("o", vec!["x"]), ("t", vec!["h"])]
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|y| y.to_string()).collect()))
            .collect();
        (names, ch)
    }

    #[test]
    fn full_names_follow_the_parent_chain_and_pick_the_right_leaf_twin() {
        let (n, ch) = tree();
        let f = full_names(&n, &ch);
        assert_eq!(f["l"], LOOT_TAG);
        assert_eq!(f["lc"], "LootGeneration.CanGenerateAsLoot.Special");
        assert_eq!(f["x"], "Lootable.CanGenerateAsLoot");
        assert_eq!(f["t"], "Type");
        let hits: Vec<_> = f.iter().filter(|(_, v)| v.as_str() == LOOT_TAG).map(|(k, _)| k.as_str()).collect();
        assert_eq!(hits, vec!["l"]);
    }

    #[test]
    fn admission_keeps_tag_or_descendant_and_strips_them() {
        let (_, ch) = tree();
        let lootable = descendants_incl(&["l".to_string()], &ch);
        assert_eq!(lootable, set(&["l", "lc"]));
        let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(admit(&v(&["h", "l"]), &lootable), Some(v(&["h"])));
        assert_eq!(admit(&v(&["lc", "t"]), &lootable), Some(v(&["t"])));
        assert_eq!(admit(&v(&["h"]), &lootable), None);
        // the twin record under `Lootable` does not admit
        assert_eq!(admit(&v(&["x", "h"]), &lootable), None);
        // nothing left after the removal: dropped
        assert_eq!(admit(&v(&["l", "lc"]), &lootable), None);
    }

    #[test]
    fn point_tags_drop_the_two_markers_and_compare_full_names() {
        let (n, ch) = tree();
        let mut t = Tags::new(n.clone(), vec![]);
        t.full = full_names(&n, &ch);
        assert_eq!(point_tag_names(&t, &["l".to_string(), "t".to_string()]), set(&[LOOT_TAG, "Type"]));
        let mut n2 = n.clone();
        n2.insert("m".into(), "SpawnPoint".into());
        n2.insert("hv".into(), "Harvestable".into());
        let mut ch2 = ch.clone();
        ch2.insert("hv".into(), vec!["m".into()]);
        let mut t2 = Tags::new(n2.clone(), vec![]);
        t2.full = full_names(&n2, &ch2);
        assert_eq!(point_tag_names(&t2, &["m".to_string(), "t".to_string()]), set(&["Type"]));
        // a config that requires the marker can never match; the default takes the point
        let groups = vec![(false, vec!["Harvestable.SpawnPoint".to_string()]), (true, vec![])];
        assert_eq!(pick_group(&groups, &point_tag_names(&t2, &["m".to_string()])), Some(1));
    }

    fn set(v: &[&str]) -> HashSet<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn pool_filters_fold_left_to_right() {
        let no_fac = json!({"t": "tags", "pos": [], "neg": ["fac"]});
        let no_ev = json!({"t": "tags", "pos": [], "neg": ["ev"]});
        let orb = json!({"t": "tags", "pos": ["orb"], "neg": []});
        let generic = json!({"t": "seq", "f": [{"or": false, "x": no_fac}, {"or": false, "x": no_ev}]});
        assert!(filter_passes(&generic, &set(&["gun"])));
        assert!(!filter_passes(&generic, &set(&["gun", "ev"])));
        assert!(!filter_passes(&generic, &set(&["fac"])));
        // (no faction AND no event) OR orbageddon: an event-tagged item passes only through the OR term
        let orbageddon = json!({"t": "seq", "f": [{"or": false, "x": generic.clone()}, {"or": true, "x": orb}]});
        assert!(filter_passes(&orbageddon, &set(&["ev", "orb"])));
        assert!(!filter_passes(&orbageddon, &set(&["ev"])));
        // positive tags are ALL required; an empty sequence and an unknown type reject
        assert!(!filter_passes(&json!({"t": "tags", "pos": ["a", "b"], "neg": []}), &set(&["a"])));
        assert!(!filter_passes(&json!({"t": "seq", "f": []}), &set(&[])));
        assert!(!filter_passes(&json!({"t": "unknown"}), &set(&[])));
    }

    #[test]
    fn most_specific_config_wins_over_file_order() {
        let g = |d: bool, t: &[&str]| (d, t.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        // file order: broad config first; the two-tag config must still win when the point carries both
        let groups = vec![g(false, &["Harvestable.SpawnPoint"]), g(false, &["Harvestable.SpawnPoint", "Big"]), g(false, &[]), g(true, &[])];
        assert_eq!(pick_group(&groups, &set(&["Harvestable.SpawnPoint", "Big"])), Some(1));
        assert_eq!(pick_group(&groups, &set(&["Harvestable.SpawnPoint"])), Some(0));
        // ties keep authoring order; a tagless non-default config is ignored; no match falls to the default
        let tie = vec![g(false, &["a"]), g(false, &["b"]), g(true, &[])];
        assert_eq!(pick_group(&tie, &set(&["a", "b"])), Some(0));
        assert_eq!(pick_group(&tie, &set(&["z"])), Some(2));
        assert_eq!(pick_group(&[g(false, &["a"])], &set(&["z"])), None);
    }

    /// Real archive: how many entity classes carry a `LootGenerationComponentParams` table / a fixed loadout (validates the `lootclass` readers on
    /// the whole class population, not only on the slots' crates).  Skips without the archive.
    #[test]
    fn real_archive_class_loot_population() {
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = P4k::open(std::path::Path::new(&path)) else { return };
        let Ok(items) = ItemMaterials::load(&p4k) else { return };
        let Some(d) = items.datacore() else { return };
        let r = Reader { items: &items, d, tags: Tags::new(HashMap::new(), vec![]), fills: RefCell::default() };
        let names: Vec<String> = d.record_index().keys().filter(|n| n.starts_with(ENTITY)).cloned().collect();
        let (mut with_loot, mut with_fixed, mut with_cfg, mut unresolved) = (0, 0, 0, 0);
        let mut examples = vec![];
        for n in &names {
            let cls = &n[ENTITY.len()..];
            let Some(c) = r.class_fill(cls) else { continue };
            with_cfg += 1;
            if c.loot.is_some() {
                with_loot += 1;
                if examples.len() < 12 {
                    examples.push(cls.to_string());
                }
            }
            with_fixed += !c.fixed.is_empty() as usize;
            unresolved += !c.unresolved.is_empty() as usize;
        }
        eprintln!("{} entity classes read, {} with a class loot table, {} with fixed items, {} with unresolved loadout parts; e.g. {:?}", with_cfg, with_loot, with_fixed, unresolved, examples);
        assert!(with_cfg > 10000 && with_fixed > 100);
    }

    #[test]
    fn ancestors_close_over_the_children_lists() {
        let ch: HashMap<String, Vec<String>> = [("type", vec!["armor", "weapon"]), ("armor", vec!["helmet"])].iter().map(|(k, v)| (k.to_string(), v.iter().map(|x| x.to_string()).collect())).collect();
        let a = ancestors_of(&ch);
        let mut h = a["helmet"].clone();
        h.sort();
        assert_eq!(h, vec!["armor", "type"]);
        assert_eq!(a["weapon"], vec!["type"]);
        assert!(!a.contains_key("type"));
    }

    /// Real archive (one at a time, ~2 GB): every pool entry of the SecurityOutpost table (armor caches, heavy weapons, grenade launchers) is non-empty
    /// with the default tag mode, and the survey of empty entries per mode is printed.  Skips without the archive.
    #[test]
    fn real_archive_security_outpost_pools_are_not_empty() {
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = P4k::open(std::path::Path::new(&path)) else { return };
        let Ok(items) = ItemMaterials::load(&p4k) else { return };
        let Some(d) = items.datacore() else { return };
        for (label, mode) in [
            ("explicit + volume rule", TagMode { ancestors: false, volume: true, admission: false }),
            ("ancestors, no volume rule", TagMode { ancestors: true, volume: false, admission: false }),
            ("ancestors + volume rule, no admission", TagMode { ancestors: true, volume: true, admission: false }),
            ("ancestors + volume + admission (default)", TagMode::default()),
        ] {
            let r = Reader { items: &items, d, tags: read_tags_mode(d, mode), fills: RefCell::default() };
            let mut wanted = HashSet::new();
            wanted.insert("LootTable.SecurityOutpost_Loot".to_string());
            let tb = tables(&r, &wanted);
            let ents: Vec<&Value> = arr(g(&tb["LootTable.SecurityOutpost_Loot"], "arch")).iter().flat_map(|a| arr(g(g(a, "body"), "entries"))).collect();
            let empty = ents.iter().filter(|e| e["n"].as_u64() == Some(0)).count();
            eprintln!("{}: SecurityOutpost entries {} empty {} sizes {:?}", label, ents.len(), empty, ents.iter().map(|e| e["n"].as_u64().unwrap_or(0)).collect::<Vec<_>>());
            let all: HashSet<String> = d.record_index().keys().filter(|n| n.starts_with("LootTable.") || n.starts_with("LootTableV3Record.")).cloned().collect();
            let tb = tables(&r, &all);
            let (mut n, mut e0) = (0, 0);
            for tt in tb.as_object().unwrap().values() {
                for a in arr(g(tt, "arch")) {
                    for en in arr(g(g(a, "body"), "entries")) {
                        n += 1;
                        e0 += (en["n"].as_u64() == Some(0)) as usize;
                    }
                }
            }
            eprintln!("{}: indexed {} classes, not lootable {}, loot tag found {}", label, r.tags.n_items(), r.tags.excluded_not_lootable, r.tags.loot_tag_found);
            eprintln!("{}: all {} tables, {} entries, {} empty", label, all.len(), n, e0);
            if mode.ancestors && mode.volume {
                assert!(!ents.is_empty() && empty == 0);
            }
        }
    }

    /// Real archive (one at a time): the whole index with and without admission; prints the stats (indexed classes, empty pool entries, points whose
    /// group changed because the two markers are no longer added).  Admission must not empty more pools than the ancestor-only index.
    #[test]
    fn real_archive_index_admission_and_point_rule() {
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = P4k::open(std::path::Path::new(&path)) else { return };
        let Ok(items) = ItemMaterials::load(&p4k) else { return };
        let mut empty = vec![];
        for adm in [false, true] {
            let j = build_mode(&items, &p4k, TagMode { ancestors: true, volume: true, admission: adm });
            let st = &j["stats"];
            eprintln!(
                "admission {}: indexed {} not_lootable {} entries {} empty {} points {} changed {} unmatched {}",
                adm, st["items_indexed"], st["items_excluded_not_lootable"], st["pool_entries"], st["pool_entries_empty"], st["points"],
                st["points_group_changed_by_marker_rule"], st["points_unmatched"]
            );
            empty.push(st["pool_entries_empty"].as_u64().unwrap_or(0));
        }
        assert!(empty[1] <= empty[0] + 5, "admission empties pools: {:?}", empty);
    }
}
