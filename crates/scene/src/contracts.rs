//! Contracts (missions) of Game2.dcb: who offers them, where, what they require and what they pay, and which blueprints they award.
//!
//! The data model (EMPIRICAL on the live archive, 4.x DataCore).  Nothing here is confirmed against the engine; semantics marked
//! INFERRED are read from names and data shape.
//!
//! Two systems coexist:
//!
//! * `ContractGenerator` (110 records under `libs/foundry/records/contracts/contractgenerator/<guild>/<organisation>/`).  `generators[]`
//!   holds handlers: `ContractGeneratorHandler_Career` (adds `factionReputation` + `reputationScope` and per contract `minStanding` /
//!   `maxStanding`), `_List`, `_TutorialSeriesDef`, `_PVPBountyDef`, `_ServiceBeacon`, `_LinearSeries` and `_Legacy` (`legacyContracts`
//!   whose `ContractLegacy.missionBrokerEntry` points at an old-style entry).  Every handler has `defaultAvailability` and
//!   `contractParams`.  A contract (`Contract`, `CareerContract`, `ContractLegacy`) has an `id` GUID, a `debugName`, a `template`, its
//!   own `paramOverrides`, `additionalPrerequisites`, `subContracts` (the same contract offered in another locality; folded into the
//!   contract as variants) and `contractResults`.
//! * `MissionBrokerEntry` (2,584; the older system, still used by bounty, mercenary, delivery and story missions): explicit `title` /
//!   `description` / `missionGiver` strings, `type`, `lawfulMission`, `missionReward` (a fixed aUEC amount), reputation rewards and
//!   requirements, `requiredMissions`, `completionTags` / `requiredCompletedMissionTags`.
//!
//! How a generator contract's values are found (EMPIRICAL; the override order is INFERRED from the names: template, then the handler's
//! `contractParams`, then the contract's `paramOverrides`, later wins):
//!
//! * Strings: `ContractTemplate.contractDisplayInfo.displayString[ContractStringParamType]` (enum order Title, TitleHUD, Description,
//!   Contractor, CommsChannelName), overridden by `stringParamOverrides{param, value}`.  Text holds `~mission(Token)` placeholders that
//!   the game fills from the contract's properties; string-hash properties with fixed options are substituted here, the others are
//!   shown as `[Token]`.
//! * Flags: `contractDisplayInfo.illegal` and `contractClass.additionalParams.canBeShared` of the template, the handler's
//!   `defaultAvailability`; `boolParamOverrides` / `intParamOverrides` use the same names with a capital first letter.
//! * Type: `missionTypeOverride` of the contract, else of the handler, else `contractDisplayInfo.type`.
//! * Requirements: `ContractPrerequisite_*` in `defaultAvailability.prerequisites` and `additionalPrerequisites`.
//! * Rewards (`contractResults`, kept when `missionResults[EMissionResult.Completed]` is set; EMissionResult = Completed, InProgress,
//!   Failed, Abandoned, Withdrawn): `ContractResult_CalculatedReward` (the aUEC amount is NOT in the data; INFERRED to be computed by
//!   the server from `timeToComplete` and `difficulty`), `ContractResult_Reward`, `..._LegacyReputation`, `..._CalculatedReputation`,
//!   `BlueprintRewards{chance, blueprintPool}`, `..._Item`, `..._ItemsWeighting`, `..._BadgeAward`, `..._JournalEntry`,
//!   `..._ScenarioProgress`, `..._CompletionTags`, `..._RefundBuyIn`, `..._VehicleRental`.  `MissionModifier_TemporaryBlueprint` lends a
//!   blueprint for the mission.
//! * Deliveries: `MissionPropertyValue_HaulingOrders` (`HaulingOrderContent_Resource` / `_EntityClass`; brokers use
//!   `ObjectiveHandler_Hauling.haulingOrders`).
//! * Places: `StarMapObject.parent` leads to the system's star.  Location properties search by tag; a tag that is some
//!   `StarMapObject.locationHierarchyTag` names that object (EMPIRICAL: 90 such tags).
//!
//! NPC ships spawned by a contract.  Static data only; nothing here says where or when a ship is
//! placed in a running game.
//!
//! * CONFIRMED (as data shape, on the live archive): a contract property `MissionPropertyValue_ShipSpawnDescriptions{spawnDescriptions[],
//!   allowedForMissionRestrictedDeliveries}` (declared in `ContractTemplate.contractProperties`, a handler's or a contract's
//!   `propertyOverrides`, or a `MissionBrokerEntry.properties`) holds `SpawnDescription_ShipGroup{Name, embeddedName, ships[]}`; each
//!   `ships[]` entry is a `SpawnDescription_ShipOptions{options[]}` of `SpawnDescription_Ship{tags, negativeTags, markupTags, entityTags,
//!   concurrentAmount, includeLocationAISpawnTags, nameProperty, weight, initialDamageSettings}`.  A ship is NOT named by an entity class:
//!   the option carries `TagList`s of `Tag` references.  Sibling types `MissionPropertyValue_NPCSpawnDescriptions` (people) and
//!   `..._EntitySpawnDescriptions` (items / props) share the layout and are not read here.
//! * EMPIRICAL: 1,357 of 5,158 contracts (946 generator, 411 broker) declare at least one non-empty ship group; the walk of the complete
//!   records finds exactly the same 477 records.  The `positive tags` mix vocabularies: ship model / size / role tags (`Hornet`, `Medium`,
//!   `CombatShip`, `Criminal`, `Civilians` ... also found on `EntityClassDefinition.tags` of ships, AI variants such as
//!   `AEGS_Gladius_PU_AI_CRIM` carry `Criminal`, `PU`, `CombatRaiting2`), pilot skill tags (`HumanPilot20`, `AcePilot`: `SkillDefinitions` /
//!   `CrewManifest` records), cargo tags (`Full Cargo`, `LowValue`: `CargoManifest`) and mission tags (`Target`, `Bounty`, `ArriveViaQT`).
//!   Requiring all of them matches no ship in 631 of 693 (then-known) queries.
//! * INFERRED: the candidates of an option are the entity classes tagged `Ship` or `Vehicle` that carry every positive tag some such
//!   entity carries and none of the negative tags; when no ship carries them all, the ships carrying the most (`fit` "partial", 100 of
//!   886 queries).  Tags are compared by GUID (several tags share a name), without the `Tag.children` hierarchy (it changes the result in 1
//!   of 693 queries).  The game's own selection (which of the candidates, how pilot / cargo tags feed the crew and cargo manifests, what
//!   `markupTags` / `entityTags` do) is not known; a group `Name` ("Targets", "ShipToDefend", "Wave1") is the only role information
//!   and `role_hint` guesses from it.  A template declares the groups for every objective it can build, so a contract may list groups
//!   its objectives never use.  A `ships[]` entry is probably one spawn slot with a weighted pick (the field is named `weight`).
//!
//! Blueprints: `BlueprintPoolRecord.blueprintRewards[] = BlueprintReward{weight, blueprintRecord}` (see crafting.rs; the weighted pick is
//! server side, CONFIRMED there).

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::dynv::*;
use crate::itemmaterials::ItemMaterials;
use crate::itemstats::ItemStats;

pub const CONTRACT_STRUCTS: [&str; 3] = ["Contract", "CareerContract", "ContractLegacy"];
pub const STRING_PARAMS: [&str; 5] = ["Title", "TitleHUD", "Description", "Contractor", "CommsChannelName"]; // enum ContractStringParamType (EMPIRICAL order)
pub const RESULT_NAMES: [&str; 5] = ["completed", "in progress", "failed", "abandoned", "withdrawn"];
pub const NO_TEXT: [&str; 4] = ["@LOC_EMPTY", "@LOC_UNINITIALIZED", "@LOC_PLACEHOLDER", ""];
pub const DIFFICULTY_KEYS: [&str; 4] = ["mechanicalSkill", "mentalLoad", "riskOfLoss", "gameKnowledge"];
pub const DEPTH: usize = 24; // read depth of contract records; deeper parts are read on demand (Reader::o)

/// 'FactionReputation_Lawful_RedWindLinehaul' -> 'Red Wind Linehaul' style label from a record short name.
pub fn pretty(name: &str) -> String {
    let re = Regex::new(r"^(FactionReputation_(Lawful_|Unlawful_)?|MissionGiver_|Faction_(Reputation_)?(Lawful_|Unlawful_)?)").unwrap();
    let t = re.replace(name, "").replace('_', " ");
    let cs: Vec<char> = t.chars().collect();
    let mut sp = String::new();
    for i in 0..cs.len() {
        if i > 0 {
            let (a, b) = (cs[i - 1], cs[i]);
            let c1 = a.is_ascii_lowercase() && (b.is_ascii_uppercase() || b.is_ascii_digit());
            let c2 = a.is_ascii_uppercase() && b.is_ascii_uppercase() && cs.get(i + 1).map_or(false, |c| c.is_ascii_lowercase());
            if c1 || c2 {
                sp.push(' ');
            }
        }
        sp.push(cs[i]);
    }
    Regex::new(r"\s+").unwrap().replace_all(&sp, " ").trim().to_string()
}

/// Weighted mean of the four 1..8 difficulty levels (the trailing number of each enum value) with the profile's weights; None when
/// incomplete.  INFERRED meaning: an input of the server-side reward calculation.
pub fn difficulty_score(diff: &Value, weights: &Value) -> Option<f64> {
    if !truthy(diff) || !truthy(weights) {
        return None;
    }
    let re = Regex::new(r"_(\d+)$").unwrap();
    let (mut tot, mut wsum) = (0.0, 0.0);
    for k in DIFFICULTY_KEYS {
        let m = re.captures(s(g(diff, k)))?;
        let w = num(g(weights, &format!("{}Weight", k)))?;
        tot += m[1].parse::<f64>().ok()? * w;
        wsum += w;
    }
    if wsum != 0.0 {
        Some(pyround(tot / wsum, 2))
    } else {
        None
    }
}

/// Replace `~mission(Token)` / `~mission(Token|...)` by `tokens[Token]` (case-insensitive) or `[Token]`.
pub fn fill_tokens(text: &str, tokens: &[(String, String)]) -> String {
    if text.is_empty() {
        return String::new();
    }
    let re = Regex::new(r"~mission\(([^)]*)\)").unwrap();
    re.replace_all(text, |c: &regex::Captures| {
        let k = c[1].split('|').next().unwrap().trim().to_string();
        let v = tokens.iter().rev().find(|(t, _)| t.to_lowercase() == k.to_lowercase()).map(|x| x.1.clone());
        match v {
            Some(v) if !v.is_empty() => v,
            _ => format!("[{}]", k),
        }
    })
    .to_string()
}

fn is_ref(v: &Value) -> bool {
    matches!(v, Value::Array(a) if a.len() == 3 && a[0] == "ref" && a[1].as_u64() != Some(0xFFFF_FFFF))
}

fn push(v: &mut Value, key: &str, item: Value) {
    v[key].as_array_mut().unwrap().push(item);
}

/// GUIDs of the tag references in a `TagList` (`{tags: [ref]}`).
pub fn tag_guids(list: &Value) -> Vec<String> {
    arr(g(list, "tags")).iter().filter(|r| is_ref(r)).filter_map(|r| r[2].as_str().map(String::from)).collect()
}

fn contains_str(v: &Value, key: &str, x: &str) -> bool {
    arr(&v[key]).iter().any(|y| y.as_str() == Some(x))
}

/// Reads contracts out of a DataCore.  Needs the guid -> record map and the localisation (both through `ItemMaterials` / `ItemStats`).
pub struct Reader<'a> {
    pub items: &'a ItemMaterials,
    pub stats: &'a ItemStats<'a>,
    by_struct: HashMap<String, Vec<usize>>,
    memo: Mutex<HashMap<(usize, usize), Option<Arc<Value>>>>,
    memo2: Mutex<HashMap<String, Arc<Value>>>,
    pool: OnceLock<ShipPool>,
    tag_names: Mutex<HashMap<String, String>>,
    /// Ship tag queries seen so far, `{query id: {tags, not, match, extra, cands}}` (shared by all options that use the same query).
    queries: Mutex<BTreeMap<String, Value>>,
}

/// CONFIRMED against the engine in 4.10.1: the ship search adds the current game mode's tag as required, and the engine's failure text
/// names it: "An entity class is probably missing the AI.Spawning.GameMode.PU tag".  INFERRED: the leaf
/// tag named `PU` on entity classes is that tag.  So plain ship classes never spawn for a PU mission, only their `*_PU_AI_*` variants.
const GAME_MODE_TAG: &str = "PU";

/// The ships a spawn tag query can pick from: entity classes whose `tags` include a tag named `Ship` or `Vehicle`, with their tag GUIDs,
/// and the union of those tags (`vocab`).  A query tag outside the vocabulary names no ship property (pilot skill, cargo, mission role).
pub struct ShipPool {
    pub ents: Vec<(String, HashSet<String>)>,
    pub vocab: HashSet<String>,
}

/// Cap on stored candidates per query (the page shows far fewer).
pub const MAX_CANDIDATES: usize = 400;

/// Outcome of matching one tag query against the ship pool.
pub struct Resolved {
    /// "all" (candidates carry every ship tag), "partial" (no ship does; the candidates carry the most), "none" (no ship tag or no ship).
    pub fit: &'static str,
    /// GUIDs of the positive tags that some pool ship carries.
    pub matched: Vec<String>,
    /// GUIDs of the other positive tags (pilot skill, cargo, mission role ...: no ship carries them).
    pub extra: Vec<String>,
    pub cands: Vec<String>,
}

impl ShipPool {
    /// INFERRED matching rule: a spawn option's `tags` are ANDed against an entity class's `tags`, `negativeTags` exclude.  Tags no ship
    /// carries (the crew and cargo manifests and the mission use them) are set aside, as the query mixes the vocabularies of several
    /// databases (EMPIRICAL: with them required 631 of 693 queries matched nothing).
    pub fn resolve(&self, pos: &[String], neg: &[String]) -> Resolved {
        let (matched, extra): (Vec<String>, Vec<String>) = pos.iter().cloned().partition(|t| self.vocab.contains(t));
        let score = |tags: &HashSet<String>| matched.iter().filter(|t| tags.contains(*t)).count();
        let allowed: Vec<&(String, HashSet<String>)> = self.ents.iter().filter(|(_, tags)| !neg.iter().any(|t| tags.contains(t))).collect();
        let best = allowed.iter().map(|(_, tags)| score(tags)).max().unwrap_or(0);
        let (fit, want) = if matched.is_empty() || best == 0 {
            ("none", usize::MAX)
        } else if best == matched.len() {
            ("all", best)
        } else {
            ("partial", best)
        };
        let cands = allowed.iter().filter(|(_, tags)| score(tags) == want).map(|(c, _)| c.clone()).take(MAX_CANDIDATES).collect();
        Resolved { fit, matched, extra, cands }
    }
}

/// What [`ship_groups`] needs from the DataCore (implemented by [`Reader`]; unit tests use a stub).
pub trait SpawnSource {
    /// Structs behind a value (one dict or an array of them, depth-limit tuples read on demand).
    fn dicts(&self, v: &Value) -> Vec<Value>;
    /// `tagName` for a tag GUID.
    fn tag(&self, guid: &str) -> String;
    /// Id of the registered tag query.
    fn query(&self, pos: &[String], neg: &[String]) -> String;
    /// Localised text of a key, or null.
    fn text(&self, key: &Value) -> Value;
    /// Short record name behind a reference.
    fn short(&self, r: &Value) -> Option<String>;
}

/// The groups of one `MissionPropertyValue_ShipSpawnDescriptions` property `p` (`{missionVariableName, value}`): per
/// `SpawnDescription_ShipGroup` its slots (`SpawnDescription_ShipOptions`; INFERRED from the names: one weighted pick per slot) of
/// options `{q, weight, count, markup, entity_tags, name_prop, damage}`.  `layer` says where the property was declared (template, handler,
/// contract, locality variant, broker entry).  Empty descriptions give no group.
pub fn ship_groups(src: &dyn SpawnSource, p: &Value, layer: &str) -> Vec<Value> {
    let Some(v) = src.dicts(g(p, "value")).into_iter().next() else { return vec![] };
    if struct_of(&v) != "MissionPropertyValue_ShipSpawnDescriptions" {
        return vec![];
    }
    let var = pystr(g(p, "missionVariableName"));
    let mut out = vec![];
    for gd in src.dicts(g(&v, "spawnDescriptions")) {
        if struct_of(&gd) != "SpawnDescription_ShipGroup" {
            continue;
        }
        let mut slots: Vec<Value> = vec![];
        for sl in src.dicts(g(&gd, "ships")) {
            let opts: Vec<Value> = src
                .dicts(g(&sl, "options"))
                .iter()
                .filter(|o| struct_of(o) == "SpawnDescription_Ship")
                .map(|o| {
                    let names = |k: &str| tag_guids(g(o, k)).iter().map(|t| src.tag(t)).collect::<Vec<_>>();
                    json!({"q": src.query(&tag_guids(g(o, "tags")), &tag_guids(g(o, "negativeTags"))),
                           "weight": if o.get("weight").is_some() { g(o, "weight").clone() } else { json!(1.0) },
                           "count": g(o, "concurrentAmount"), "markup": names("markupTags"), "entity_tags": names("entityTags"),
                           "name_prop": g(o, "nameProperty"), "damage": src.short(g(o, "initialDamageSettings"))})
                })
                .collect();
            if !opts.is_empty() {
                slots.push(Value::Array(opts));
            }
        }
        if !slots.is_empty() {
            out.push(json!({"var": var, "layer": layer, "name": g(&gd, "Name"), "label": src.text(g(&gd, "embeddedName")), "slots": slots}));
        }
    }
    out
}

impl SpawnSource for Reader<'_> {
    fn dicts(&self, v: &Value) -> Vec<Value> {
        match self.o(v) {
            Some(one) => vec![one.into_owned()], // a struct, or a depth-limit tuple read now
            None => Reader::dicts(self, v).into_iter().map(|c| c.into_owned()).collect(),
        }
    }
    fn tag(&self, guid: &str) -> String {
        self.tag_by_guid(guid)
    }
    fn query(&self, pos: &[String], neg: &[String]) -> String {
        self.ship_query(pos, neg)
    }
    fn text(&self, key: &Value) -> Value {
        self.jtext(key)
    }
    fn short(&self, r: &Value) -> Option<String> {
        Reader::short(self, r)
    }
}

impl<'a> Reader<'a> {
    pub fn new(items: &'a ItemMaterials, stats: &'a ItemStats<'a>) -> Self {
        Reader { items, stats, by_struct: crate::crafting::records_by_struct(items), memo: Mutex::new(HashMap::new()), memo2: Mutex::new(HashMap::new()),
                 pool: OnceLock::new(), tag_names: Mutex::new(HashMap::new()), queries: Mutex::new(BTreeMap::new()) }
    }

    fn d(&self) -> &p4k_formats::dcb::Dcb {
        self.items.datacore().expect("DataCore")
    }

    // ---- plumbing
    /// A decoded struct: dicts pass through; `["class" | "strong", struct, index]` tuples left by the read depth limit are read now.  Weak
    /// pointers are never followed (they point back up the tree; following them is what makes deep reads explode).
    pub fn o<'v>(&self, v: &'v Value) -> Option<Cow<'v, Value>> {
        if v.is_object() {
            return Some(Cow::Borrowed(v));
        }
        if let Value::Array(a) = v {
            if a.len() == 3 && matches!(a[0].as_str(), Some("class") | Some("strong")) && a[1].as_u64() != Some(0xFFFF_FFFF) {
                let (si, idx) = (a[1].as_u64()? as usize, a[2].as_u64()? as usize);
                return self.d().read_instance(si, idx, 0, 12).ok().map(|x| Cow::Owned(x.to_json()));
            }
        }
        None
    }

    pub fn dicts<'v>(&self, v: &'v Value) -> Vec<Cow<'v, Value>> {
        if v.is_object() {
            return vec![Cow::Borrowed(v)];
        }
        arr(v).iter().filter_map(|y| self.o(y)).collect()
    }

    pub fn records(&self, st: &str) -> &[usize] {
        self.by_struct.get(st).map_or(&[], Vec::as_slice)
    }

    pub fn rindex(&self, r: &Value) -> Option<usize> {
        if is_ref(r) {
            self.items.guid_record(r[2].as_str()?)
        } else {
            None
        }
    }

    pub fn rname(&self, r: &Value) -> Option<String> {
        let i = self.rindex(r)?;
        self.d().record_name(i).ok().map(|n| n.into_owned())
    }

    pub fn short(&self, r: &Value) -> Option<String> {
        self.rname(r).map(|n| n.split_once('.').map_or(n.clone(), |x| x.1.to_string()))
    }

    fn jshort(&self, r: &Value) -> Value {
        self.short(r).map_or(Value::Null, |x| json!(x))
    }

    /// Record data behind a reference (memoised), or None.
    pub fn rdata(&self, r: &Value, depth: usize) -> Option<Arc<Value>> {
        let i = self.rindex(r)?;
        let key = (i, depth);
        if let Some(v) = self.memo.lock().unwrap().get(&key) {
            return v.clone();
        }
        let d = self.d();
        let v = d.record(i).ok().and_then(|rc| d.record_data(&rc, depth).ok()).map(|v| Arc::new(v.to_json()));
        self.memo.lock().unwrap().insert(key, v.clone());
        v
    }

    fn rd(&self, r: &Value, depth: usize) -> Arc<Value> {
        self.rdata(r, depth).unwrap_or_else(|| Arc::new(Value::Null))
    }

    pub fn text(&self, key: &Value) -> Option<String> {
        let k = key.as_str()?;
        if NO_TEXT.contains(&k) {
            return None;
        }
        if !k.starts_with('@') {
            return Some(k.to_string());
        }
        self.stats.localise_str(k).filter(|t| !t.is_empty() && !t.contains("PLACEHOLDER"))
    }

    fn jtext(&self, key: &Value) -> Value {
        self.text(key).map_or(Value::Null, |t| json!(t))
    }

    // ---- labelled records
    /// `{id, label}` of a record with a localised name field; the label falls back to the prettified record name.
    pub fn labelled(&self, r: &Value, field: &str) -> Value {
        let Some(sh) = self.short(r) else { return Value::Null };
        let rec = self.rd(r, 1);
        json!({"id": sh, "label": self.text(g(&rec, field)).unwrap_or_else(|| pretty(&sh))})
    }

    pub fn standing(&self, r: &Value) -> Value {
        let Some(sh) = self.short(r) else { return Value::Null };
        let rec = self.rd(r, 1);
        let label = self.text(g(&rec, "displayName")).or_else(|| Some(s(g(&rec, "name")).to_string()).filter(|x| !x.is_empty())).unwrap_or_else(|| sh.clone());
        json!({"id": sh, "label": label, "min": g(&rec, "minReputation")})
    }

    pub fn scope(&self, r: &Value) -> Value {
        let Some(sh) = self.short(r) else { return Value::Null };
        let rec = self.rd(r, 1);
        let label = self.text(g(&rec, "displayName")).or_else(|| Some(s(g(&rec, "scopeName")).to_string()).filter(|x| !x.is_empty())).unwrap_or_else(|| sh.clone());
        json!({"id": sh, "label": label})
    }

    pub fn mission_type(&self, r: &Value) -> Value {
        self.labelled(r, "LocalisedTypeName")
    }

    pub fn tag_name(&self, r: &Value) -> Option<String> {
        let rec = self.rdata(r, 1)?;
        Some(s(g(&rec, "tagName")).to_string()).filter(|x| !x.is_empty())
    }

    /// `{id, label, system}` of a StarMapObject: the system is the label of the root of its parent chain (a star).
    pub fn place(&self, r: &Value) -> Option<Value> {
        let sh = self.short(r)?;
        let key = format!("place:{}", sh);
        if let Some(v) = self.memo2.lock().unwrap().get(&key) {
            return Some((**v).clone());
        }
        let rec = self.rd(r, 1);
        let mut root = sh.clone();
        let mut cur: Arc<Value> = rec.clone();
        let mut seen = 0;
        while truthy(&cur) && is_ref(g(&cur, "parent")) && seen < 30 {
            let parent = g(&cur, "parent").clone();
            root = self.short(&parent).unwrap_or(root);
            cur = self.rd(&parent, 1);
            seen += 1;
        }
        let from_chain = if truthy(&cur) { self.text(g(&cur, "name")) } else { None };
        let system = from_chain.filter(|x| !x.is_empty()).unwrap_or_else(|| Regex::new(r"(Star|SolarSystem)$").unwrap().replace(&root, "").to_string());
        let system = Regex::new(r"\s+System$").unwrap().replace(system.trim(), "").to_string(); // EMPIRICAL: some star names read "Stanton System", the fallback "Stanton"
        let label = self.text(g(&rec, "name")).unwrap_or_else(|| pretty(&sh));
        let v = json!({"id": sh, "label": label.trim(), "system": system});
        self.memo2.lock().unwrap().insert(key, Arc::new(v.clone()));
        Some(v)
    }

    pub fn locality(&self, r: &Value) -> Vec<Value> {
        let rec = self.rd(r, 1);
        arr(g(&rec, "availableLocations")).iter().filter_map(|x| self.place(x)).collect()
    }

    /// `{tag record name: [place]}` for every StarMapObject.locationHierarchyTag (EMPIRICAL: how location tags name places).
    pub fn tag_places(&self) -> Arc<Value> {
        if let Some(v) = self.memo2.lock().unwrap().get("tagplaces") {
            return v.clone();
        }
        let d = self.d();
        let mut out = Map::new();
        for &i in self.records("StarMapObject") {
            let Ok(rec) = d.record(i) else { continue };
            let Ok(data) = d.record_data(&rec, 1) else { continue };
            let data = data.to_json();
            let t = g(&data, "locationHierarchyTag");
            if let Some(tn) = self.rname(t) {
                if let Some(p) = self.place(&json!(["ref", 0, rec.guid])) {
                    out.entry(tn).or_insert_with(|| json!([])).as_array_mut().unwrap().push(p);
                }
            }
        }
        let v = Arc::new(Value::Object(out));
        self.memo2.lock().unwrap().insert("tagplaces".into(), v.clone());
        v
    }

    /// `[{bp, weight}]` of a BlueprintPoolRecord.
    pub fn blueprint_pool(&self, r: &Value) -> Vec<Value> {
        let rec = self.rd(r, 3);
        let mut out = vec![];
        for x in self.dicts(g(&rec, "blueprintRewards")) {
            if let Some(b) = self.short(g(&x, "blueprintRecord")) {
                out.push(json!({"bp": b, "weight": if x.get("weight").is_some() { g(&x, "weight").clone() } else { json!(1.0) }}));
            }
        }
        out
    }

    /// `[{pool, minPoints, faction, bps}]` of the reputation tiers of a ScenarioProgress record.
    pub fn scenario_pools(&self, r: &Value) -> Vec<Value> {
        let rec = self.rd(r, 12);
        let mut out = vec![];
        for t in self.dicts(g(&rec, "factionRewardTiers")) {
            let fac = self.jshort(g(&t, "faction"));
            for tp in self.dicts(g(&t, "tierProgressions")) {
                for tr in self.dicts(g(&tp, "tierRewards")) {
                    for p in arr(g(&tr, "blueprintPool")) {
                        if let Some(n) = self.short(p) {
                            out.push(json!({"pool": n, "minPoints": g(&tr, "minPoints"), "faction": fac, "bps": self.blueprint_pool(p)}));
                        }
                    }
                }
            }
        }
        out
    }

    // ---- spawned ships
    /// `tagName` of the Tag record with this GUID (memoised).
    fn tag_by_guid(&self, guid: &str) -> String {
        if let Some(n) = self.tag_names.lock().unwrap().get(guid) {
            return n.clone();
        }
        let n = self.tag_name(&json!(["ref", 0, guid])).unwrap_or_else(|| guid.to_string());
        self.tag_names.lock().unwrap().insert(guid.to_string(), n.clone());
        n
    }

    fn ship_pool(&self) -> &ShipPool {
        self.pool.get_or_init(|| {
            let d = self.d();
            let (mut ents, mut vocab) = (vec![], HashSet::new());
            for &i in self.records("EntityClassDefinition") {
                let Ok(rec) = d.record(i) else { continue };
                let Ok(data) = d.record_data(&rec, 1) else { continue };
                let data = data.to_json();
                let guids: HashSet<String> = tag_guids(&json!({"tags": g(&data, "tags")})).into_iter().collect();
                if !guids.iter().any(|t| matches!(self.tag_by_guid(t).as_str(), "Ship" | "Vehicle")) {
                    continue;
                }
                vocab.extend(guids.iter().cloned());
                if guids.iter().any(|t| self.tag_by_guid(t) == GAME_MODE_TAG) {
                    ents.push((rec.name.split_once('.').map_or(rec.name.clone(), |x| x.1.to_string()), guids));
                }
            }
            ents.sort_by(|a, b| a.0.cmp(&b.0));
            ShipPool { ents, vocab }
        })
    }

    /// Register the tag query `pos` / `neg` (tag GUIDs) and return its id.  The candidates are the pool ships that carry every positive tag
    /// of the ship vocabulary and none of the negative tags (INFERRED matching rule, see the module docs).
    fn ship_query(&self, pos: &[String], neg: &[String]) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new(); // fixed keys: the id is stable between runs
        (pos, neg).hash(&mut h);
        let id = format!("{:016x}", h.finish());
        if self.queries.lock().unwrap().contains_key(&id) {
            return id;
        }
        let r = self.ship_pool().resolve(pos, neg);
        let names = |l: &[String]| l.iter().map(|t| self.tag_by_guid(t)).collect::<Vec<_>>();
        let v = json!({"tags": names(pos), "not": names(neg), "match": names(&r.matched), "extra": names(&r.extra), "fit": r.fit, "cands": r.cands});
        self.queries.lock().unwrap().insert(id.clone(), v);
        id
    }

    /// Fold a `MissionPropertyValue_ShipSpawnDescriptions` property into `row["ships"]` (see [`ship_groups`]).
    pub fn ship_prop(&self, p: &Value, layer: &str, row: &mut Value) {
        for grp in ship_groups(self, p, layer) {
            push(row, "ships", grp);
        }
    }

    /// The tag queries collected so far (`{id: query}`).
    pub fn queries(&self) -> BTreeMap<String, Value> {
        self.queries.lock().unwrap().clone()
    }

    // ---- shared pieces
    /// Fold `ContractResults.contractResults` into the reward dict `out` (only results set for Completed; others are listed in
    /// `out['other']` with the outcomes they apply to).
    pub fn rewards(&self, results: &Value, out: &mut Value) {
        for r in self.dicts(results) {
            let st = struct_of(&r).to_string();
            let flags_v = g(&r, "missionResults");
            let flags: Vec<Value> = if truthy(flags_v) { arr(flags_v).to_vec() } else { vec![json!(true)] };
            let on: Vec<&str> = flags.iter().enumerate().filter(|(i, fl)| truthy(fl) && *i < RESULT_NAMES.len()).map(|(i, _)| RESULT_NAMES[i]).collect();
            let kind = st.replace("ContractResult_", "");
            if flags.is_empty() || !truthy(&flags[0]) {
                push(out, "other", json!({"kind": kind, "on": on}));
                continue;
            }
            match st.as_str() {
                "ContractResult_CalculatedReward" => out["uec_calc"] = json!(true),
                "ContractResult_Reward" => self.money(g(&r, "contractReward"), out),
                "ContractResult_LegacyReputation" => self.rep_amounts(&json!([g(&r, "contractResultReputationAmounts")]), out),
                "ContractResult_CalculatedReputation" => {
                    let f_ = self.labelled(g(&r, "factionReputation"), "displayName");
                    if truthy(&f_) {
                        push(out, "rep", json!({"faction": f_, "scope": self.scope(g(&r, "reputationScope")), "amount": null}));
                    }
                }
                "BlueprintRewards" => {
                    if let Some(pool) = self.short(g(&r, "blueprintPool")) {
                        push(out, "blueprints", json!({"pool": pool, "chance": if r.get("chance").is_some() { g(&r, "chance").clone() } else { json!(1.0) }, "bps": self.blueprint_pool(g(&r, "blueprintPool"))}));
                    }
                }
                "ContractResult_Item" => {
                    if let Some(cls) = self.short(g(&r, "entityClass")) {
                        push(out, "items", self.item(&cls, if r.get("amount").is_some() { g(&r, "amount").clone() } else { json!(1) }));
                    }
                }
                "ContractResult_ItemsWeighting" => {
                    for w in self.dicts(g(&r, "itemAwardStructure")) {
                        let mut awards: Vec<Cow<Value>> = self.dicts(g(&w, "awards")).into_iter().map(|c| Cow::Owned(c.into_owned())).collect();
                        let rec = if is_ref(g(&w, "awardsRecord")) { self.rdata(g(&w, "awardsRecord"), 6) } else { None };
                        if let Some(rec) = rec.filter(|r| truthy(r)) {
                            let a = g(&rec, "itemAwardStructure");
                            let wl = if truthy(a) { a } else { g(&rec, "weightings") };
                            for ww in self.dicts(wl) {
                                awards.extend(self.dicts(g(&ww, "awards")).into_iter().map(|c| Cow::Owned(c.into_owned())));
                            }
                        }
                        let its: Vec<Value> = awards
                            .iter()
                            .filter_map(|a| {
                                let c = self.short(g(a, "entityClass"))?;
                                Some(self.item(&c, if a.get("amountToAward").is_some() { g(a, "amountToAward").clone() } else { json!(1) }))
                            })
                            .collect();
                        if !its.is_empty() {
                            push(out, "item_sets", json!({"weight": if w.get("weighting").is_some() { g(&w, "weighting").clone() } else { json!(1.0) }, "items": its}));
                        }
                    }
                }
                "ContractResult_BadgeAward" => push(out, "badges", g(&r, "badgeToAward").clone()),
                "ContractResult_ScenarioProgress" => {
                    let pl = g(&r, "scenarioProgressPlugin");
                    if let Some(sp) = self.short(g(pl, "scenarioProgressRecord")) {
                        push(out, "scenario", json!({"record": sp, "points": g(&r, "PointsToAward"), "pools": self.scenario_pools(g(pl, "scenarioProgressRecord"))}));
                    }
                }
                "ContractResult_CompletionTags" => {
                    for t in self.dicts(g(&r, "completionTags")) {
                        if let Some(n) = self.rname(g(&t, "tag")) {
                            push(out, "tags", json!(n));
                        }
                    }
                }
                "ContractResult_JournalEntry" => {
                    for j in arr(g(&r, "journalEntriesToAdd")) {
                        if let Some(x) = self.short(j) {
                            push(out, "journal", json!(x));
                        }
                    }
                }
                "ContractResult_VehicleRental" => {
                    if let Some(cls) = self.short(g(&r, "vehicleEntityClass")) {
                        let mut it = self.item(&cls, json!(1));
                        it["rental"] = json!(true);
                        push(out, "items", it);
                    }
                }
                "ContractResult_RefundBuyIn" => out["refund"] = json!(true),
                _ => push(out, "other", json!({"kind": kind, "on": on})),
            }
        }
    }

    pub fn money(&self, mr: &Value, out: &mut Value) {
        let amt = if truthy(g(mr, "reward")) { g(mr, "reward").clone() } else { json!(0) };
        let mx = if truthy(g(mr, "max")) { g(mr, "max").clone() } else { json!(0) };
        if truthy(&amt) || truthy(&mx) {
            out["uec"] = json!({"min": amt, "max": if f(&mx) > f(&amt) { mx.clone() } else { amt.clone() },
                                "currency": if truthy(g(mr, "currencyType")) { g(mr, "currencyType").clone() } else { json!("UEC") }, "bonus": truthy(g(mr, "plusBonuses"))});
        }
    }

    pub fn rep_amounts(&self, lst: &Value, out: &mut Value) {
        for a in self.dicts(lst) {
            let f_ = self.labelled(g(&a, "factionReputation"), "displayName");
            if !truthy(&f_) {
                continue;
            }
            let amt = self.rdata(g(&a, "reward"), 1).map_or(Value::Null, |r| g(&r, "reputationAmount").clone());
            push(out, "rep", json!({"faction": f_, "scope": self.scope(g(&a, "reputationScope")), "amount": amt}));
        }
    }

    pub fn item(&self, cls: &str, amount: Value) -> Value {
        let e = self.stats.identity(cls);
        let name = if truthy(&e["name"]) { e["name"].clone() } else { json!(cls) };
        json!({"cls": cls, "name": name, "type": if truthy(&e["type"]) { e["type"].clone() } else { json!("") }, "amount": amount})
    }

    pub fn new_rewards() -> Value {
        json!({"uec": null, "uec_calc": false, "rep": [], "blueprints": [], "items": [], "item_sets": [], "badges": [], "scenario": [],
               "tags": [], "journal": [], "refund": false, "other": [], "temp_blueprints": []})
    }

    /// Fold ContractPrerequisite_* structs into `row` (crime, rep_req, tag_req, places).
    pub fn prereqs(&self, lst: &Value, row: &mut Value) {
        for p in self.dicts(lst) {
            let st = struct_of(&p);
            match st {
                "ContractPrerequisite_CrimeStat" => row["crime"] = json!({"min": g(&p, "minCrimeStat"), "max": g(&p, "maxCrimeStat")}),
                "ContractPrerequisite_Reputation" => {
                    let f_ = self.labelled(g(&p, "factionReputation"), "displayName");
                    if truthy(&f_) {
                        push(row, "rep_req", json!({"faction": f_, "scope": self.scope(g(&p, "scope")), "min": self.standing(g(&p, "minStanding")),
                                                    "max": self.standing(g(&p, "maxStanding")), "exclude": truthy(g(&p, "exclude"))}));
                    }
                }
                "ContractPrerequisite_CompletedContractTags" => {
                    let req: Vec<String> = arr(g(g(&p, "requiredCompletedContractTags"), "tags")).iter().filter_map(|t| self.rname(t)).collect();
                    let exc: Vec<String> = arr(g(g(&p, "excludedCompletedContractTags"), "tags")).iter().filter_map(|t| self.rname(t)).collect();
                    if !req.is_empty() || !exc.is_empty() {
                        push(row, "tag_req", json!({"tags": req, "count": if p.get("requiredCountValue").is_some() { g(&p, "requiredCountValue").clone() } else { json!(1) },
                                                    "exclude": exc, "exclude_count": if p.get("excludedCountValue").is_some() { g(&p, "excludedCountValue").clone() } else { json!(1) }}));
                    }
                }
                "ContractPrerequisite_Locality" => {
                    if let Some(n) = self.short(g(&p, "localityAvailable")) {
                        if !contains_str(row, "localities", &n) {
                            push(row, "localities", json!(n));
                            let pl = self.locality(g(&p, "localityAvailable"));
                            Self::add_places(row, &pl);
                        }
                    }
                }
                "ContractPrerequisite_Location" => {
                    if let Some(pl) = self.place(g(&p, "locationAvailable")) {
                        Self::add_places(row, &[pl]);
                    }
                }
                "" => {}
                other => push(row, "other_req", json!(other.replace("ContractPrerequisite_", ""))),
            }
        }
    }

    pub fn add_places(row: &mut Value, places: &[Value]) {
        for p in places {
            let label = s(&p["label"]);
            if !contains_str(row, "places", label) {
                push(row, "places", json!(label));
            }
            if truthy(&p["system"]) && !contains_str(row, "systems", s(&p["system"])) {
                push(row, "systems", p["system"].clone());
            }
        }
    }

    /// Read MissionProperty values: text tokens, contractor organisations, location tags, hauling orders, mission items.
    pub fn properties(&self, props: &Value, row: &mut Value, tokens: &mut Vec<(String, String)>) {
        let tp = self.tag_places();
        for p in self.dicts(props) {
            let Some(v) = self.o(g(&p, "value")) else { continue };
            let st = struct_of(&v);
            let tok = s(g(&p, "extendedTextToken")).to_string();
            match st {
                "MissionPropertyValue_StringHash" if !tok.is_empty() => {
                    let mut opts: Vec<String> = vec![];
                    for o in self.dicts(g(&v, "options")) {
                        if let Some(t) = self.text(g(&o, "textId")).filter(|t| !t.is_empty()) {
                            if !opts.contains(&t) {
                                opts.push(t);
                            }
                        }
                    }
                    if !opts.is_empty() {
                        set_token(tokens, &tok, opts.join(" / "));
                    }
                }
                "MissionPropertyValue_Integer" if !tok.is_empty() => {
                    let vals: Vec<Value> = self.dicts(g(&v, "options")).iter().map(|o| g(o, "value").clone()).collect();
                    if !vals.is_empty() {
                        let mut uniq: Vec<&Value> = vec![];
                        for x in &vals {
                            if !uniq.iter().any(|u| jeq(u, x)) {
                                uniq.push(x);
                            }
                        }
                        set_token(tokens, &tok, uniq.iter().map(|x| pystr(x)).collect::<Vec<_>>().join(" / "));
                        if tok == "ScripAmount" {
                            row["scrip"] = vals[0].clone();
                        }
                    }
                }
                "MissionPropertyValue_Organization" => {
                    for m in self.dicts(g(&v, "matchConditions")) {
                        for o in arr(g(&m, "organizations")) {
                            if let Some(n) = self.short(o) {
                                if !contains_str(row, "orgs", &n) {
                                    push(row, "orgs", json!(n));
                                }
                            }
                        }
                    }
                }
                "MissionPropertyValue_Location" | "MissionPropertyValue_Locations" => {
                    for m in self.dicts(g(&v, "matchConditions")) {
                        for t in self.dicts(g(&m, "tagSearch")) {
                            for x in arr(g(&t, "positiveTags")) {
                                let n = self.rname(x);
                                if let Some(places) = n.as_ref().and_then(|n| tp.get(n.as_str())) {
                                    let pl: Vec<Value> = arr(places).to_vec();
                                    Self::add_places(row, &pl);
                                } else if n.is_some() {
                                    if let Some(tn) = self.tag_name(x) {
                                        if !contains_str(row, "loc_tags", &tn) {
                                            push(row, "loc_tags", json!(tn));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                "MissionPropertyValue_HaulingOrders" => self.hauling(g(&v, "haulingOrderContent"), row),
                "MissionPropertyValue_MissionItem" => {
                    for m in self.dicts(g(&v, "matchConditions")) {
                        for it in arr(g(&m, "items")) {
                            if let Some(n) = self.short(it) {
                                if !contains_str(row, "mission_items", &n) {
                                    push(row, "mission_items", json!(n));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn hauling(&self, lst: &Value, row: &mut Value) {
        for h in self.dicts(lst) {
            let st = struct_of(&h);
            if st == "HaulingOrderContent_Resource" || st == "HaulingOrder_Resource" {
                if let Some(n) = self.short(g(&h, "resource")) {
                    let lab = self.labelled(g(&h, "resource"), "displayName");
                    push(row, "haul", json!({"kind": "resource", "ref": n, "name": if truthy(&lab) { lab["label"].clone() } else { json!(n) }, "min": g(&h, "minSCU"), "max": g(&h, "maxSCU")}));
                }
            } else if st == "HaulingOrderContent_EntityClass" || st == "HaulingOrder_EntityClass" {
                if let Some(n) = self.short(g(&h, "entityClass")) {
                    let amount = first_truthy_v(&[g(&h, "amount"), g(&h, "minAmount")]);
                    let mut it = self.item(&n, amount);
                    it["kind"] = json!("item");
                    it["ref"] = json!(n);
                    push(row, "haul", it);
                }
            } else if st == "HaulingOrder_OrOption_And" || h.get("orders").is_some() {
                self.hauling(g(&h, "orders"), row);
            } else if h.get("haulingOrders").is_some() {
                self.hauling(g(&h, "haulingOrders"), row);
            }
        }
    }

    pub fn new_row(cid: Value, src: &str, record: &str) -> Value {
        json!({"id": cid, "src": src, "record": record, "generator": null, "handler": null, "debug": "", "title": "", "desc": "",
               "contractor": "", "orgs": [], "giver": null, "faction": null, "type": null, "illegal": false, "shareable": false,
               "once": false, "cooldown": null, "max_players": null, "buy_in": 0, "time": null, "difficulty": null, "systems": [],
               "places": [], "localities": [], "loc_tags": [], "crime": null, "rep_req": [], "tag_req": [], "other_req": [],
               "requires": [], "scenarios": [], "rewards": Self::new_rewards(), "haul": [], "mission_items": [], "scrip": null,
               "dev": false, "variants": 0, "event": null, "ships": [], "ship_n": 0})
    }

    // ---- generator contracts
    fn template(&self, r: &Value) -> Arc<Value> {
        let key = format!("tpl:{}", if is_ref(r) { r[2].as_str().unwrap_or("") } else { "" });
        if let Some(v) = self.memo2.lock().unwrap().get(&key) {
            return v.clone();
        }
        let rec = self.rd(r, DEPTH);
        let di = g(&rec, "contractDisplayInfo");
        let cc = g(&rec, "contractClass");
        let ds = arr(g(di, "displayString"));
        let mut strings = Map::new();
        for i in 0..ds.len().min(STRING_PARAMS.len()) {
            strings.insert(STRING_PARAMS[i].into(), ds[i].clone());
        }
        let v = Arc::new(json!({
            "strings": strings, "type": g(di, "type"), "illegal": truthy(g(di, "illegal")),
            "shareable": truthy(g(g(cc, "additionalParams"), "canBeShared")),
            "deadline": g(g(g(cc, "autoFinishSettings"), "contractDeadline"), "missionCompletionTime"),
            "props": if truthy(g(&rec, "contractProperties")) { g(&rec, "contractProperties").clone() } else { json!([]) },
            "modifiers": if truthy(g(&rec, "modifiers")) { g(&rec, "modifiers").clone() } else { json!([]) },
            "dev": truthy(g(&rec, "notForRelease")), "name": self.jshort(r)
        }));
        self.memo2.lock().unwrap().insert(key, v.clone());
        v
    }

    /// Rows of the contracts of ContractGenerator record `i` and `{broker record name: generator info}` for its legacy contracts.
    pub fn generator(&self, i: usize) -> (Vec<Value>, Vec<(String, Value)>) {
        let d = self.d();
        let rec = d.record(i).expect("record");
        let gdata = d.record_data(&rec, DEPTH).map(|v| v.to_json()).unwrap_or(Value::Null);
        let (mut rows, mut legacy) = (vec![], vec![]);
        for h in self.dicts(g(&gdata, "generators")) {
            for (key, val) in h.as_object().unwrap() {
                if !val.is_array() {
                    continue;
                }
                for c in self.dicts(val) {
                    let st = struct_of(&c);
                    if !CONTRACT_STRUCTS.contains(&st) {
                        continue;
                    }
                    if st == "ContractLegacy" {
                        if let Some(b) = self.rname(g(&c, "missionBrokerEntry")) {
                            legacy.push((b, json!({"generator": rec.name, "handler": g(&h, "debugName"), "id": g(&c, "id")})));
                        }
                        continue;
                    }
                    rows.push(self.contract(&c, &h, &rec, key));
                }
            }
        }
        (rows, legacy)
    }

    fn contract(&self, c: &Value, h: &Value, rec: &p4k_formats::dcb::Record, key: &str) -> Value {
        let tpl = self.template(g(c, "template"));
        let cid = if truthy(g(c, "id")) { g(c, "id").clone() } else { json!(format!("{}/{}", rec.name, pystr(g(c, "debugName")))) };
        let mut row = Self::new_row(cid, "generator", &rec.name);
        row["generator"] = json!(rec.name.split_once('.').map_or(rec.name.as_str(), |x| x.1));
        row["handler"] = g(h, "debugName").clone();
        row["debug"] = if truthy(g(c, "debugName")) { g(c, "debugName").clone() } else { json!("") };
        row["list"] = json!(key);
        row["template"] = tpl["name"].clone();
        row["dev"] = json!(truthy(g(h, "notForRelease")) || truthy(g(c, "notForRelease")) || truthy(g(c, "workInProgress")) || truthy(g(h, "workInProgress")));
        let (hp, cp) = (g(h, "contractParams"), g(c, "paramOverrides"));
        let mut strings: Vec<(String, Value)> = tpl["strings"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for layer in [hp, cp] {
            for sp in self.dicts(g(layer, "stringParamOverrides")) {
                let k = s(g(&sp, "param")).to_string();
                match strings.iter_mut().find(|(n, _)| *n == k) {
                    Some(slot) => slot.1 = g(&sp, "value").clone(),
                    None => strings.push((k, g(&sp, "value").clone())),
                }
            }
        }
        let sget = |k: &str| strings.iter().find(|(n, _)| n == k).map_or(Value::Null, |x| x.1.clone());
        let av = g(h, "defaultAvailability");
        let mut flags: Vec<(String, Value)> = vec![("illegal".into(), tpl["illegal"].clone()), ("canBeShared".into(), tpl["shareable"].clone())];
        let setf = |flags: &mut Vec<(String, Value)>, k: &str, v: Value| match flags.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = v,
            None => flags.push((k.to_string(), v)),
        };
        for k in ["onceOnly", "maxPlayersPerInstance", "hasPersonalCooldown", "personalCooldownTime", "availableInPrison", "hideInMobiGlas"] {
            if av.get(k).is_some() {
                setf(&mut flags, k, g(av, k).clone());
            }
        }
        for layer in [hp, cp] {
            for kind in ["boolParamOverrides", "intParamOverrides"] {
                for bp in self.dicts(g(layer, kind)) {
                    let n = s(g(&bp, "param"));
                    let mut ch = n.chars();
                    let name = match ch.next() {
                        Some(c0) => c0.to_lowercase().collect::<String>() + ch.as_str(),
                        None => String::new(),
                    };
                    setf(&mut flags, &name, g(&bp, "value").clone());
                }
            }
        }
        let fl = |k: &str| flags.iter().find(|(n, _)| n == k).map_or(Value::Null, |x| x.1.clone());
        row["illegal"] = json!(truthy(&fl("illegal")));
        row["shareable"] = json!(truthy(&fl("canBeShared")));
        row["once"] = json!(truthy(&fl("onceOnly")));
        row["max_players"] = fl("maxPlayersPerInstance");
        row["cooldown"] = if truthy(&fl("hasPersonalCooldown")) { fl("personalCooldownTime") } else { Value::Null };
        let mt = if is_ref(g(cp, "missionTypeOverride")) {
            g(cp, "missionTypeOverride")
        } else if is_ref(g(hp, "missionTypeOverride")) {
            g(hp, "missionTypeOverride")
        } else {
            &tpl["type"]
        };
        row["type"] = self.mission_type(mt);
        // faction (career handlers) and requirements
        row["faction"] = self.labelled(g(h, "factionReputation"), "displayName");
        self.prereqs(g(av, "prerequisites"), &mut row);
        self.prereqs(g(c, "additionalPrerequisites"), &mut row);
        if struct_of(c) == "CareerContract" && truthy(&row["faction"]) {
            let (mn, mx) = (self.standing(g(c, "minStanding")), self.standing(g(c, "maxStanding")));
            if truthy(&mn) || truthy(&mx) {
                let e = json!({"faction": row["faction"], "scope": self.scope(g(h, "reputationScope")), "min": mn, "max": mx, "exclude": false});
                row["rep_req"].as_array_mut().unwrap().insert(0, e);
            }
        }
        let scen: Vec<Value> = arr(g(h, "required_active_scenarios")).iter().chain(arr(g(c, "required_active_scenarios")).iter()).filter_map(|x| self.short(x)).map(|x| json!(x)).collect();
        row["scenarios"] = Value::Array(scen);
        // properties (template, handler, contract; later ones replace earlier ones with the same variable / token)
        let mut merged: Vec<((String, String), Value, &str)> = vec![];
        for (name, layer) in [("template", &tpl["props"]), ("handler", g(hp, "propertyOverrides")), ("contract", g(cp, "propertyOverrides"))] {
            for p in self.dicts(layer) {
                let k = (pystr(g(&p, "missionVariableName")), pystr(g(&p, "extendedTextToken")));
                match merged.iter_mut().find(|(n, _, _)| *n == k) {
                    Some(slot) => {
                        slot.1 = p.into_owned();
                        slot.2 = name;
                    }
                    None => merged.push((k, p.into_owned(), name)),
                }
            }
        }
        let mut tokens: Vec<(String, String)> = vec![];
        for (_, p, layer) in &merged {
            self.ship_prop(p, layer, &mut row);
        }
        let mvals = Value::Array(merged.into_iter().map(|x| x.1).collect());
        self.properties(&mvals, &mut row, &mut tokens);
        for sc in self.dicts(g(c, "subContracts")) {
            row["variants"] = json!(row["variants"].as_i64().unwrap() + 1);
            self.prereqs(g(&sc, "additionalPrerequisites"), &mut row);
            for p in self.dicts(g(&sc, "propertyOverrides")) {
                self.ship_prop(&p, "variant", &mut row);
            }
            self.properties(g(&sc, "propertyOverrides"), &mut row, &mut vec![]);
        }
        // results
        let cr = g(c, "contractResults");
        let mut rw = row["rewards"].take();
        self.rewards(g(cr, "contractResults"), &mut rw);
        row["rewards"] = rw;
        row["buy_in"] = if truthy(g(cr, "contractBuyInAmount")) { g(cr, "contractBuyInAmount").clone() } else { json!(0) };
        row["time"] = if truthy(g(cr, "timeToComplete")) { g(cr, "timeToComplete").clone() } else { Value::Null };
        let diff = g(cr, "difficulty");
        if diff.is_object() {
            let prof = self.rd(g(diff, "difficultyProfile"), 1);
            let mut dm = Map::new();
            for k in DIFFICULTY_KEYS {
                dm.insert(k.into(), g(diff, k).clone());
            }
            dm.insert("profile".into(), self.jshort(g(diff, "difficultyProfile")));
            dm.insert("score".into(), jopt(difficulty_score(diff, &prof)));
            row["difficulty"] = Value::Object(dm);
        }
        let mut mods: Vec<Value> = arr(&tpl["modifiers"]).to_vec();
        mods.extend(arr(g(hp, "modifierOverrides")).iter().cloned());
        mods.extend(arr(g(cp, "modifierOverrides")).iter().cloned());
        for m in self.dicts(&Value::Array(mods)) {
            if struct_of(&m) == "MissionModifier_TemporaryBlueprint" {
                if let Some(pool) = self.short(g(&m, "blueprintPool")) {
                    let bps = self.blueprint_pool(g(&m, "blueprintPool"));
                    let mut rw = row["rewards"].take();
                    push(&mut rw, "temp_blueprints", json!({"pool": pool, "bps": bps}));
                    row["rewards"] = rw;
                }
            }
        }
        let contractor = self.text(&sget("Contractor")).unwrap_or_default();
        if !tokens.iter().any(|(k, _)| k == "Contractor") {
            tokens.push(("Contractor".into(), contractor.clone()));
        }
        row["contractor"] = json!(contractor);
        row["title_key"] = sget("Title");
        let title = fill_tokens(&self.text(&sget("Title")).unwrap_or_default(), &tokens);
        row["title"] = if title.is_empty() { row["debug"].clone() } else { json!(title) };
        row["desc"] = json!(fill_tokens(&self.text(&sget("Description")).unwrap_or_default(), &tokens));
        row
    }

    // ---- broker entries
    pub fn broker(&self, i: usize) -> Value {
        let d = self.d();
        let rec = d.record(i).expect("record");
        let b = d.record_data(&rec, DEPTH).map(|v| v.to_json()).unwrap_or(Value::Null);
        let mut row = Self::new_row(if truthy(&json!(rec.guid)) { json!(rec.guid) } else { json!(rec.name) }, "broker", &rec.name);
        row["debug"] = json!(rec.name.split_once('.').map_or(rec.name.as_str(), |x| x.1));
        row["dev"] = json!(truthy(g(&b, "notForRelease")));
        row["type"] = self.mission_type(g(&b, "type"));
        row["illegal"] = json!(!(if b.get("lawfulMission").is_some() { truthy(g(&b, "lawfulMission")) } else { true }));
        row["shareable"] = json!(truthy(g(&b, "canBeShared")));
        row["once"] = json!(truthy(g(&b, "onceOnly")));
        row["max_players"] = g(&b, "maxPlayersPerInstance").clone();
        row["cooldown"] = if truthy(g(&b, "hasPersonalCooldown")) { g(&b, "personalCooldownTime").clone() } else { Value::Null };
        row["buy_in"] = if truthy(g(&b, "missionBuyInAmount")) { g(&b, "missionBuyInAmount").clone() } else { json!(0) };
        let dl = g(g(&b, "missionDeadline"), "missionCompletionTime");
        row["time"] = if truthy(dl) && truthy(g(g(&b, "missionDeadline"), "missionAutoEnd")) { json!(pyround(f(dl) / 60.0, 1)) } else { Value::Null };
        if let Some(gr) = self.rdata(g(&b, "missionGiverRecord"), 1) {
            let sh = self.short(g(&b, "missionGiverRecord")).unwrap_or_default();
            row["giver"] = json!({"id": sh, "label": self.text(g(&gr, "displayName")).unwrap_or_else(|| pretty(&sh))});
            row["faction"] = self.labelled(g(&gr, "reputation"), "displayName");
        }
        if let Some(pl) = self.place(g(&b, "locationMissionAvailable")) {
            Self::add_places(&mut row, &[pl]);
        }
        if let Some(loc) = self.short(g(&b, "localityAvailable")) {
            push(&mut row, "localities", json!(loc));
            let pl = self.locality(g(&b, "localityAvailable"));
            Self::add_places(&mut row, &pl);
        }
        let wl = g(g(&b, "reputationPrerequisites"), "wantedLevel");
        if truthy(wl) {
            row["crime"] = json!({"min": g(wl, "minValue"), "max": g(wl, "maxValue")});
        }
        for e in self.dicts(g(g(&b, "reputationRequirements"), "expression")) {
            let f_ = self.labelled(g(&e, "factionReputation"), "displayName");
            if truthy(&f_) {
                let st = self.standing(g(&e, "standing"));
                let cmp = s(g(&e, "comparison")).to_string();
                push(&mut row, "rep_req", json!({"faction": f_, "scope": self.scope(g(&e, "reputationScope")), "cmp": cmp,
                    "min": if ["GreaterThanOrEqualTo", "GreaterThan", "EqualTo"].contains(&cmp.as_str()) { st.clone() } else { Value::Null },
                    "max": if ["LessThanOrEqualTo", "LessThan", "EqualTo"].contains(&cmp.as_str()) { st } else { Value::Null }, "exclude": false}));
            }
        }
        row["requires"] = Value::Array(arr(g(&b, "requiredMissions")).iter().filter_map(|r| self.guid_of(r)).map(|x| json!(x)).collect());
        for t in self.dicts(g(&b, "requiredCompletedMissionTags")) {
            let req: Vec<String> = arr(g(&t, "positiveTags")).iter().filter_map(|x| self.rname(x)).collect();
            let exc: Vec<String> = arr(g(&t, "negativeTags")).iter().filter_map(|x| self.rname(x)).collect();
            if !req.is_empty() || !exc.is_empty() {
                push(&mut row, "tag_req", json!({"tags": req, "count": 1, "exclude": exc, "exclude_count": 1}));
            }
        }
        let mut rw = row["rewards"].take();
        self.money(g(&b, "missionReward"), &mut rw);
        let rr = arr(g(&b, "missionResultReputationRewards"));
        if let Some(r0) = rr.first().filter(|r| r.is_object()) {
            self.rep_amounts(g(r0, "reputationAmounts"), &mut rw);
        }
        rw["tags"] = Value::Array(arr(g(g(&b, "completionTags"), "tags")).iter().filter_map(|t| self.rname(t)).map(|x| json!(x)).collect());
        rw["journal"] = Value::Array(arr(g(&b, "journalEntriesToAddOnComplete")).iter().filter_map(|j| self.short(j)).map(|x| json!(x)).collect());
        row["rewards"] = rw;
        let mut tokens: Vec<(String, String)> = vec![];
        for p in self.dicts(g(&b, "properties")) {
            self.ship_prop(&p, "broker", &mut row);
        }
        self.properties(g(&b, "properties"), &mut row, &mut tokens);
        self.broker_hauling(&b, &mut row, 0);
        if truthy(g(&b, "availableDateSchedule")) {
            let sch = self.dicts(g(&b, "availableDateSchedule"));
            if let Some(sc) = sch.first() {
                let dt = |se: &str, k: &str| pystr(g(g(g(sc, se), "date"), k));
                row["event"] = json!(format!(
                    "{} {} - {} {}{}",
                    dt("start", "day"),
                    dt("start", "month"),
                    dt("end", "day"),
                    dt("end", "month"),
                    if s(g(sc, "repeatType")) == "RepeatYearly" { " (yearly)" } else { "" }
                ));
            }
        }
        let giver_text = self.text(g(&b, "missionGiver")).unwrap_or_default();
        row["contractor"] = json!(giver_text);
        if !tokens.iter().any(|(k, _)| k == "Contractor") {
            tokens.push(("Contractor".into(), giver_text));
        }
        row["title_key"] = g(&b, "title").clone();
        let title = fill_tokens(&self.text(g(&b, "title")).unwrap_or_default(), &tokens);
        row["title"] = if title.is_empty() { row["debug"].clone() } else { json!(title) };
        row["desc"] = json!(fill_tokens(&self.text(g(&b, "description")).unwrap_or_default(), &tokens));
        row
    }

    /// Hauling orders anywhere in the entry (EMPIRICAL: `ObjectiveHandler_Hauling.haulingOrders` of the objectives).
    fn broker_hauling(&self, o: &Value, row: &mut Value, depth: usize) {
        if depth > 40 {
            return;
        }
        match o {
            Value::Object(m) => {
                if struct_of(o) == "ObjectiveHandler_Hauling" {
                    self.hauling(g(o, "haulingOrders"), row);
                    return;
                }
                for (k, v) in m {
                    if (v.is_object() || v.is_array()) && k != "properties" {
                        self.broker_hauling(v, row, depth + 1);
                    }
                }
            }
            Value::Array(a) => {
                for v in a {
                    if v.is_object() || v.is_array() {
                        self.broker_hauling(v, row, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn guid_of(&self, r: &Value) -> Option<String> {
        let i = self.rindex(r)?;
        let rec = self.d().record(i).ok()?;
        Some(if rec.guid.is_empty() { rec.name } else { rec.guid })
    }
}

fn set_token(tokens: &mut Vec<(String, String)>, k: &str, v: String) {
    match tokens.iter_mut().find(|(n, _)| n == k) {
        Some(slot) => slot.1 = v,
        None => tokens.push((k.to_string(), v)),
    }
}

fn first_truthy_v(c: &[&Value]) -> Value {
    c.iter().find(|v| truthy(v)).map_or_else(|| c[c.len() - 1].clone(), |v| (*v).clone())
}

/// Post-process: tag providers (who awards the tags a contract needs), the reverse 'unlocks' lists, dedup of haul entries.
pub fn link_rows(rows: &mut [Value]) {
    let mut by_tag: HashMap<String, Vec<String>> = HashMap::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        by_id.insert(pystr(&r["id"]), i);
        for t in arr(&r["rewards"]["tags"]) {
            by_tag.entry(s(t).to_string()).or_default().push(pystr(&r["id"]));
        }
    }
    for r in rows.iter_mut() {
        let id = pystr(&r["id"]);
        let mut prov: Vec<String> = vec![];
        for tr in arr(&r["tag_req"]) {
            for t in arr(&tr["tags"]) {
                for x in by_tag.get(s(t)).into_iter().flatten() {
                    if *x != id {
                        prov.push(x.clone());
                    }
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        let after: Vec<Value> = prov.into_iter().filter(|x| seen.insert(x.clone())).take(200).map(|x| json!(x)).collect();
        r["after"] = Value::Array(after);
        r["unlocks"] = json!([]);
        let mut seen_h: Vec<Value> = vec![];
        let mut haul = vec![];
        for h in arr(&r["haul"]) {
            let k = json!([h["kind"], h["ref"], h.get("min"), h.get("max")]);
            if !seen_h.iter().any(|x| jeq(x, &k)) {
                seen_h.push(k);
                haul.push(h.clone());
            }
        }
        r["haul"] = Value::Array(haul);
    }
    let mut unlocks: Vec<Vec<String>> = vec![vec![]; rows.len()];
    for r in rows.iter() {
        let id = pystr(&r["id"]);
        for x in arr(&r["requires"]).iter().chain(arr(&r["after"]).iter()) {
            if let Some(&j) = by_id.get(&pystr(x)) {
                unlocks[j].push(id.clone());
            }
        }
    }
    for (i, r) in rows.iter_mut().enumerate() {
        let mut seen = std::collections::HashSet::new();
        r["unlocks"] = Value::Array(unlocks[i].iter().filter(|x| seen.insert((*x).clone())).take(200).map(|x| json!(x)).collect());
    }
}

/// The whole contract index: `{"v", "rows": [row], "blueprints": {bp id: {out, name}}}`.  The generators are read in parallel on a small
/// pool (their records are decoded 24 levels deep, which is large).
pub fn build(items: &ItemMaterials, stats: &ItemStats) -> Value {
    let rd = Reader::new(items, stats);
    let d = items.datacore().expect("DataCore");
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    use rayon::prelude::*;
    let gens: Vec<(Vec<Value>, Vec<(String, Value)>)> = pool.install(|| rd.records("ContractGenerator").par_iter().map(|&i| rd.generator(i)).collect());
    let mut rows: Vec<Value> = vec![];
    let mut legacy: HashMap<String, Value> = HashMap::new();
    for (r, lg) in gens {
        rows.extend(r);
        for (k, v) in lg {
            legacy.insert(k, v);
        }
    }
    let brokers: Vec<Value> = pool.install(|| rd.records("MissionBrokerEntry").par_iter().map(|&i| rd.broker(i)).collect());
    for mut row in brokers {
        if let Some(gv) = legacy.get(s(&row["record"])) {
            row["generator"] = json!(s(&gv["generator"]).split_once('.').map_or(s(&gv["generator"]), |x| x.1));
            row["handler"] = gv["handler"].clone();
        }
        rows.push(row);
    }
    link_rows(&mut rows);
    let shipq = rd.queries();
    for r in rows.iter_mut() {
        r["ship_n"] = json!(ship_classes(r, &shipq).len());
    }
    let mut bps: Vec<(String, Value)> = vec![];
    let touch = |bps: &mut Vec<(String, Value)>, b: &str| {
        if !bps.iter().any(|(k, _)| k == b) {
            bps.push((b.to_string(), Value::Null));
        }
    };
    for r in &rows {
        let rw = &r["rewards"];
        for b in arr(&rw["blueprints"]).iter().chain(arr(&rw["temp_blueprints"]).iter()) {
            for x in arr(&b["bps"]) {
                touch(&mut bps, s(&x["bp"]));
            }
        }
        for sc in arr(&rw["scenario"]) {
            for p in arr(&sc["pools"]) {
                for x in arr(&p["bps"]) {
                    touch(&mut bps, s(&x["bp"]));
                }
            }
        }
    }
    let mut bp_rec: HashMap<String, usize> = HashMap::new();
    for &i in rd.records("CraftingBlueprintRecord") {
        if let Ok(n) = d.record_name(i) {
            bp_rec.insert(n.split_once('.').map_or(n.to_string(), |x| x.1.to_string()), i);
        }
    }
    let mut bpm = Map::new();
    for (b, _) in bps {
        let mut out: Option<String> = None;
        if let Some(&i) = bp_rec.get(&b) {
            if let Ok(rc) = d.record(i) {
                if let Ok(bd) = d.record_data(&rc, 4) {
                    let bd = bd.to_json();
                    out = rd.short(g(g(g(&bd, "blueprint"), "processSpecificData"), "entityClass"));
                }
            }
        }
        let name = match &out {
            Some(o) => {
                let e = stats.identity(o);
                if truthy(&e["name"]) { e["name"].clone() } else { json!(o) }
            }
            None => json!(b),
        };
        bpm.insert(b, json!({"out": out, "name": name}));
    }
    json!({"v": 2, "rows": rows, "blueprints": bpm, "shipq": shipq})
}

/// INFERRED role of a spawn group from its names (`Name` of the group, e.g. "Targets" / "ShipToDefend", and the property variable); the
/// data has no role field.  None when the names say nothing.
pub fn role_hint(name: &str, var: &str) -> Option<&'static str> {
    fn hint(text: &str) -> Option<&'static str> {
        let n = text.to_lowercase();
        let has = |ks: &[&str]| ks.iter().any(|k| n.contains(k));
        if has(&["target", "hostile", "enemy", "enemies", "ambus", "interdiction", "vultures", "pirates"]) {
            Some("hostile or target")
        } else if has(&["todefend", "defenders", "friendly", "allies", "allied", "escortship", "attackedship"]) {
            Some("friendly ship (defend / escort)")
        } else if has(&["recipient", "salvageable", "prospector", "heist", "cargoship"]) {
            Some("cargo, salvage or delivery ship")
        } else {
            None
        }
    }
    hint(name).or_else(|| hint(var)) // the group's own name first: a generic group inside a "...Targets_BP" variable still says "target"
}

/// Distinct candidate ship classes of a contract's spawn options, with how many options can pick each: `[(class, options)]`, sorted by class.
pub fn ship_classes(row: &Value, queries: &BTreeMap<String, Value>) -> Vec<(String, usize)> {
    let mut m: BTreeMap<String, usize> = BTreeMap::new();
    for grp in arr(&row["ships"]) {
        for slot in arr(&grp["slots"]) {
            for o in arr(slot) {
                for c in queries.get(s(&o["q"])).map_or(&[][..], |q| arr(&q["cands"])) {
                    *m.entry(s(c).to_string()).or_default() += 1;
                }
            }
        }
    }
    m.into_iter().collect()
}

/// Ship class -> `(contract id, exact)` for every contract that can spawn it (the "Ship contains" filter).  `exact` is false when the
/// class only comes from "partial" tag queries (no ship carries every ship tag, see [`Resolved::fit`]).
pub fn ship_index(rows: &[Value], queries: &BTreeMap<String, Value>) -> BTreeMap<String, Vec<(String, bool)>> {
    let mut out: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    for r in rows {
        let mut exact: BTreeMap<String, bool> = BTreeMap::new();
        for grp in arr(&r["ships"]) {
            for slot in arr(&grp["slots"]) {
                for o in arr(slot) {
                    let Some(q) = queries.get(s(&o["q"])) else { continue };
                    let ex = s(&q["fit"]) == "all";
                    for c in arr(&q["cands"]) {
                        let e = exact.entry(s(c).to_string()).or_insert(false);
                        *e |= ex;
                    }
                }
            }
        }
        for (cls, ex) in exact {
            out.entry(cls).or_default().push((pystr(&r["id"]), ex));
        }
    }
    out
}

/// Estimated chance that a contract brings at least one ship of `wanted` (classes), for every contract that can: `{contract id: 0..1}`.
///
/// The picks are the engine's, CONFIRMED in 4.10.1.  A slot takes one option by `weight`, the first
/// when every weight is zero.  The option then draws `concurrentAmount` ships, each uniformly among its matching classes with
/// repeats allowed, and none when the amount is 0.  So a slot gives `share * (1 - (1 - matches / candidates) ^ amount)`.
/// Still assumed: every listed group spawns.  The engine walks a property's groups in order, one per call, and how many calls a mission
/// makes is its own logic, so this is an upper bound for later waves.  Location tags (`includeLocationAISpawnTags`) can narrow the candidates
/// at run time.  Slots are independent; a variable declared by several layers counts only in its last layer (later wins, INFERRED).
/// `partial` also counts "partial" tag queries (best fit, see [`Resolved::fit`]); the engine requires every tag, so those likely never spawn.
pub fn spawn_chances(rows: &[Value], queries: &BTreeMap<String, Value>, wanted: &HashSet<String>, partial: bool) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for r in rows {
        let groups = arr(&r["ships"]);
        let last: HashMap<&str, &str> = groups.iter().map(|g| (s(&g["var"]), s(&g["layer"]))).collect();
        let mut miss = 1.0;
        for grp in groups.iter().filter(|g| last.get(s(&g["var"])) == Some(&s(&g["layer"]))) {
            for slot in arr(&grp["slots"]) {
                let opts = arr(slot);
                let w = |o: &Value| o["weight"].as_f64().unwrap_or(1.0).max(0.0);
                let total: f64 = opts.iter().map(w).sum();
                let mut hit = 0.0;
                for (k, o) in opts.iter().enumerate() {
                    let Some(q) = queries.get(s(&o["q"])) else { continue };
                    if s(&q["fit"]) != "all" && !partial {
                        continue;
                    }
                    let cands = arr(&q["cands"]);
                    if cands.is_empty() {
                        continue;
                    }
                    let n = cands.iter().filter(|c| wanted.contains(s(c))).count();
                    let share = if total > 0.0 { w(o) / total } else { (k == 0) as u8 as f64 };
                    let draws = o["count"].as_f64().unwrap_or(1.0).max(0.0);
                    hit += share * (1.0 - (1.0 - n as f64 / cands.len() as f64).powf(draws));
                }
                miss *= 1.0 - hit.min(1.0);
            }
        }
        if miss < 1.0 {
            out.insert(pystr(&r["id"]), 1.0 - miss);
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------------------------ service
/// `(blueprint id, how)` pairs of one contract: 'reward' (BlueprintRewards, with the pool chance), 'temporary' (lent for the mission),
/// 'scenario' (a reputation tier of the event the contract gives points to).
pub fn blueprint_links(row: &Value) -> Vec<(String, &'static str)> {
    let rw = &row["rewards"];
    let mut out: Vec<(String, &'static str)> = vec![];
    for b in arr(&rw["blueprints"]) {
        for x in arr(&b["bps"]) {
            out.push((s(&x["bp"]).to_string(), "reward"));
        }
    }
    for b in arr(&rw["temp_blueprints"]) {
        for x in arr(&b["bps"]) {
            out.push((s(&x["bp"]).to_string(), "temporary"));
        }
    }
    for sc in arr(&rw["scenario"]) {
        for p in arr(&sc["pools"]) {
            for x in arr(&p["bps"]) {
                out.push((s(&x["bp"]).to_string(), "scenario"));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|k| seen.insert(k.clone()));
    out
}

/// The compact row of /contracts/index.json.
pub fn summary_row(r: &Value) -> Value {
    let rw = &r["rewards"];
    let uec = &rw["uec"];
    let mut haul: Vec<String> = arr(&r["haul"]).iter().map(|h| pystr(&h["name"])).collect();
    haul.sort();
    haul.dedup();
    haul.truncate(6);
    let rank = arr(&r["rep_req"]).iter().find(|x| truthy(g(x, "min"))).map_or(json!(""), |x| x["min"]["label"].clone());
    // The sum (a missing amount counts as 0) starts at int 0, stays int while every amount is an int, else float
    // (and `0 + -0.0` is `0.0`, which Rust's own f64 sum would not give).
    let mut isum: i64 = 0;
    let mut fsum: Option<f64> = None;
    for x in arr(&rw["rep"]) {
        let a = &x["amount"];
        if !truthy(a) {
            continue;
        }
        match (a.as_i64(), fsum) {
            (Some(i), None) => isum += i,
            _ => fsum = Some(fsum.unwrap_or(isum as f64) + f(a)),
        }
    }
    let rep: Value = match fsum {
        Some(v) => json!(v),
        None => json!(isum),
    };
    json!({"id": r["id"], "title": r["title"], "src": r["src"], "type": if truthy(g(&r["type"], "label")) { r["type"]["label"].clone() } else { json!("") },
           "giver": first_truthy_v(&[g(&r["giver"], "label"), &r["contractor"], &json!("")]), "faction": if truthy(g(&r["faction"], "label")) { r["faction"]["label"].clone() } else { json!("") },
           "systems": r["systems"], "illegal": r["illegal"], "uec": if truthy(uec) { json!([uec["min"], uec["max"]]) } else { Value::Null }, "calc": rw["uec_calc"],
           "rep": rep, "bp": blueprint_links(r).len(), "items": arr(&rw["items"]).len() + arr(&rw["item_sets"]).len(),
           "share": r["shareable"], "once": r["once"], "dev": r["dev"],
           "req": truthy(&r["rep_req"]) || truthy(&r["tag_req"]) || truthy(&r["requires"]), "rank": rank,
           "haul": haul, "diff": g(&r["difficulty"], "score"), "time": r["time"], "ships": r["ship_n"]})
}

/// Lookups over a built index (`Contracts.set_data`).
pub struct Contracts {
    pub data: Value,
    shipq: BTreeMap<String, Value>,
    by_id: HashMap<String, usize>,
    by_bp: HashMap<String, Vec<(String, &'static str)>>,
    by_res: HashMap<String, Vec<String>>,
}

impl Contracts {
    pub fn new(data: Value) -> Self {
        let (mut by_id, mut by_bp, mut by_res): (HashMap<String, usize>, HashMap<String, Vec<(String, &'static str)>>, HashMap<String, Vec<String>>) = Default::default();
        for (i, r) in arr(&data["rows"]).iter().enumerate() {
            let id = pystr(&r["id"]);
            by_id.insert(id.clone(), i);
            for (b, how) in blueprint_links(r) {
                by_bp.entry(b).or_default().push((id.clone(), how));
            }
            for h in arr(&r["haul"]) {
                by_res.entry(s(&h["ref"]).to_lowercase()).or_default().push(id.clone());
            }
        }
        let shipq = data["shipq"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        Contracts { data, shipq, by_id, by_bp, by_res }
    }

    /// The spawn tag queries `{id: {tags, not, match, extra, fit, cands}}` the ship options of the rows refer to.
    pub fn shipq_map(&self) -> &BTreeMap<String, Value> {
        &self.shipq
    }

    pub fn get(&self, cid: &str) -> Option<&Value> {
        self.by_id.get(cid).map(|&i| &self.data["rows"][i])
    }

    /// `[(contract id, how)]` of the contracts that award blueprint `bp`.
    pub fn for_blueprint(&self, bp: &str) -> &[(String, &'static str)] {
        self.by_bp.get(bp).map_or(&[], Vec::as_slice)
    }

    /// Contract ids whose hauling orders carry resource / entity class `r`.
    pub fn delivering(&self, r: &str) -> &[String] {
        self.by_res.get(&r.to_lowercase()).map_or(&[], Vec::as_slice)
    }

    pub fn blueprint_counts(&self) -> HashMap<&str, usize> {
        self.by_bp.iter().map(|(k, v)| (k.as_str(), v.len())).collect()
    }
}

#[cfg(test)]
mod spawn_tests {
    use super::*;

    fn tref(guid: &str) -> Value {
        json!(["ref", 1, guid])
    }

    fn tags(gs: &[&str]) -> Value {
        json!({"__struct__": "TagList", "tags": gs.iter().map(|g| tref(g)).collect::<Vec<_>>()})
    }

    /// Tag names are the GUIDs upper-cased; queries are listed in `seen`.
    struct Stub {
        seen: Mutex<Vec<(Vec<String>, Vec<String>)>>,
    }

    impl SpawnSource for Stub {
        fn dicts(&self, v: &Value) -> Vec<Value> {
            if v.is_object() {
                vec![v.clone()]
            } else {
                arr(v).to_vec()
            }
        }
        fn tag(&self, guid: &str) -> String {
            guid.to_uppercase()
        }
        fn query(&self, pos: &[String], neg: &[String]) -> String {
            let mut s_ = self.seen.lock().unwrap();
            s_.push((pos.to_vec(), neg.to_vec()));
            format!("q{}", s_.len())
        }
        fn text(&self, key: &Value) -> Value {
            key.clone()
        }
        fn short(&self, _: &Value) -> Option<String> {
            None
        }
    }

    fn option(pos: &[&str], neg: &[&str], weight: f64, n: i64) -> Value {
        json!({"__struct__": "SpawnDescription_Ship", "tags": tags(pos), "negativeTags": tags(neg), "markupTags": tags(&["m1"]), "entityTags": tags(&[]),
               "concurrentAmount": n, "weight": weight, "nameProperty": null})
    }

    fn property(groups: Vec<Value>) -> Value {
        json!({"missionVariableName": "Targets_BP", "value": {"__struct__": "MissionPropertyValue_ShipSpawnDescriptions", "spawnDescriptions": groups}})
    }

    fn group(name: &str, slots: Vec<Vec<Value>>) -> Value {
        let ships: Vec<Value> = slots.into_iter().map(|o| json!({"__struct__": "SpawnDescription_ShipOptions", "options": o})).collect();
        json!({"__struct__": "SpawnDescription_ShipGroup", "Name": name, "embeddedName": null, "ships": ships})
    }

    #[test]
    fn groups_slots_and_options_are_read() {
        let src = Stub { seen: Mutex::new(vec![]) };
        let p = property(vec![group("Targets", vec![vec![option(&["a", "b"], &["c"], 5.0, 1), option(&["a"], &[], 1.0, 2)], vec![option(&["d"], &[], 1.0, 1)]])]);
        let out = ship_groups(&src, &p, "template");
        assert_eq!(out.len(), 1);
        assert_eq!((s(&out[0]["var"]), s(&out[0]["layer"]), s(&out[0]["name"])), ("Targets_BP", "template", "Targets"));
        let slots = arr(&out[0]["slots"]);
        assert_eq!((slots.len(), arr(&slots[0]).len(), arr(&slots[1]).len()), (2, 2, 1));
        let o = &slots[0][0];
        assert_eq!((o["q"].as_str(), o["weight"].as_f64(), o["count"].as_i64()), (Some("q1"), Some(5.0), Some(1)));
        assert_eq!(o["markup"], json!(["M1"]));
        assert_eq!(slots[0][1]["count"], json!(2));
        let seen = src.seen.lock().unwrap();
        assert_eq!(seen[0], (vec!["a".to_string(), "b".to_string()], vec!["c".to_string()]));
    }

    #[test]
    fn empty_and_foreign_properties_give_nothing() {
        let src = Stub { seen: Mutex::new(vec![]) };
        assert!(ship_groups(&src, &property(vec![]), "contract").is_empty());
        assert!(ship_groups(&src, &property(vec![group("Empty", vec![])]), "contract").is_empty());
        let npc = json!({"missionVariableName": "x", "value": {"__struct__": "MissionPropertyValue_NPCSpawnDescriptions", "spawnDescriptions": [group("G", vec![vec![option(&["a"], &[], 1.0, 1)]])]}});
        assert!(ship_groups(&src, &npc, "contract").is_empty());
        assert!(src.seen.lock().unwrap().is_empty());
    }

    fn pool() -> ShipPool {
        let set = |l: &[&str]| l.iter().map(|x| x.to_string()).collect::<HashSet<_>>();
        let ents = vec![("A_Hornet_CRIM".to_string(), set(&["ship", "hornet", "crim"])), ("A_Hornet_CIV".to_string(), set(&["ship", "hornet", "civ"])),
                        ("B_Gladius_CRIM".to_string(), set(&["ship", "gladius", "crim"]))];
        let vocab = ents.iter().flat_map(|(_, t)| t.iter().cloned()).collect();
        ShipPool { ents, vocab }
    }

    fn v(l: &[&str]) -> Vec<String> {
        l.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn tag_query_matches_ships_and_sets_foreign_tags_aside() {
        let p = pool();
        let r = p.resolve(&v(&["hornet", "crim", "HumanPilot20", "Bounty"]), &[]);
        assert_eq!((r.fit, r.cands.as_slice()), ("all", &["A_Hornet_CRIM".to_string()][..]));
        assert_eq!((r.matched, r.extra), (v(&["hornet", "crim"]), v(&["HumanPilot20", "Bounty"])));
        let r = p.resolve(&v(&["crim"]), &v(&["gladius"]));
        assert_eq!(r.cands, v(&["A_Hornet_CRIM"]));
        let r = p.resolve(&v(&["ship"]), &[]);
        assert_eq!(r.cands.len(), 3);
    }

    #[test]
    fn tag_query_falls_back_to_the_best_partial_match_or_nothing() {
        let p = pool();
        let r = p.resolve(&v(&["hornet", "gladius", "crim"]), &[]);
        assert_eq!(r.fit, "partial");
        assert_eq!(r.cands, v(&["A_Hornet_CRIM", "B_Gladius_CRIM"])); // two of three tags each
        let r = p.resolve(&v(&["Target", "Bounty"]), &[]);
        assert_eq!((r.fit, r.cands.len()), ("none", 0));
        let r = p.resolve(&v(&["hornet"]), &v(&["hornet"]));
        assert_eq!((r.fit, r.cands.len()), ("none", 0));
    }

    #[test]
    fn role_hints_read_the_group_name_first() {
        assert_eq!(role_hint("Targets", "x"), Some("hostile or target"));
        assert_eq!(role_hint("ShipToDefend", "KillShip_MissionTargets_BP"), Some("friendly ship (defend / escort)"));
        assert_eq!(role_hint("Wave1", "InvisibleTimer_MissionTargets_BP"), Some("hostile or target"));
        assert_eq!(role_hint("RecipientShip", ""), Some("cargo, salvage or delivery ship"));
        assert_eq!(role_hint("", ""), None);
    }

    #[test]
    fn ship_classes_union_counts_options() {
        let mut q = BTreeMap::new();
        q.insert("q1".to_string(), json!({"cands": ["B", "A"]}));
        q.insert("q2".to_string(), json!({"cands": ["A"]}));
        let row = json!({"ships": [{"slots": [[{"q": "q1"}, {"q": "q2"}], [{"q": "q2"}, {"q": "missing"}]]}]});
        assert_eq!(ship_classes(&row, &q), vec![("A".to_string(), 3), ("B".to_string(), 1)]);
        assert!(ship_classes(&json!({"ships": []}), &q).is_empty());
    }

    #[test]
    fn spawn_chance_follows_weights_candidates_and_layers() {
        let mut q = BTreeMap::new();
        q.insert("ab".to_string(), json!({"cands": ["A", "B"], "fit": "all"}));
        q.insert("a".to_string(), json!({"cands": ["A"], "fit": "all"}));
        q.insert("c".to_string(), json!({"cands": ["C"], "fit": "all"}));
        q.insert("p".to_string(), json!({"cands": ["A"], "fit": "partial"}));
        let opt = |q: &str, w: f64| json!({"q": q, "weight": w});
        let grp = |var: &str, layer: &str, slots: Value| json!({"var": var, "layer": layer, "slots": slots});
        let want = |c: &[&str]| c.iter().map(|x| x.to_string()).collect::<HashSet<_>>();
        let chance = |ships: Value, w: &HashSet<String>, partial: bool| {
            spawn_chances(&[json!({"id": "x", "ships": ships})], &q, w, partial).get("x").copied()
        };
        let near = |a: Option<f64>, b: f64| assert!((a.unwrap() - b).abs() < 1e-9, "{:?} != {}", a, b);
        // weight 3 of 4 on an option of two candidates
        let one = json!([grp("v", "t", json!([[opt("ab", 3.0), opt("c", 1.0)]]))]);
        near(chance(one.clone(), &want(&["A"]), false), 0.375);
        // classes of one pick add up
        near(chance(one.clone(), &want(&["A", "B"]), false), 0.75);
        assert_eq!(chance(one, &want(&["Z"]), false), None);
        // two slots: at least one of them
        near(chance(json!([grp("v", "t", json!([[opt("a", 1.0), opt("c", 1.0)], [opt("a", 1.0), opt("c", 1.0)]]))]), &want(&["A"]), false), 0.75);
        // the later layer of a variable replaces the earlier one; another variable still counts
        let layered = json!([grp("v", "template", json!([[opt("a", 1.0)]])), grp("v", "contract", json!([[opt("c", 1.0)]]))]);
        assert_eq!(chance(layered, &want(&["A"]), false), None);
        // partial queries only on request; with all weights zero the first option is taken
        let part = json!([grp("v", "t", json!([[opt("p", 1.0)]]))]);
        assert_eq!(chance(part.clone(), &want(&["A"]), false), None);
        near(chance(part, &want(&["A"]), true), 1.0);
        near(chance(json!([grp("v", "t", json!([[opt("a", 0.0), opt("c", 0.0)]]))]), &want(&["A"]), false), 1.0);
        assert_eq!(chance(json!([grp("v", "t", json!([[opt("c", 0.0), opt("a", 0.0)]]))]), &want(&["A"]), false), None);
        // the amount is that many draws with repeats; 0 spawns nothing
        let amount = |n: i64| json!([grp("v", "t", json!([[{"q": "ab", "weight": 1.0, "count": n}]]))]);
        near(chance(amount(2), &want(&["A"]), false), 0.75);
        assert_eq!(chance(amount(0), &want(&["A"]), false), None);
    }

    #[test]
    fn ship_index_marks_partial_only_classes() {
        let mut q = BTreeMap::new();
        q.insert("q1".to_string(), json!({"cands": ["A", "B"], "fit": "all"}));
        q.insert("q2".to_string(), json!({"cands": ["B", "C"], "fit": "partial"}));
        let rows = vec![json!({"id": "c1", "ships": [{"slots": [[{"q": "q1"}, {"q": "q2"}]]}]}), json!({"id": "c2", "ships": []})];
        let m = ship_index(&rows, &q);
        assert_eq!(m["A"], vec![("c1".to_string(), true)]);
        assert_eq!(m["B"], vec![("c1".to_string(), true)]);
        assert_eq!(m["C"], vec![("c1".to_string(), false)]);
    }

    #[test]
    fn summary_row_carries_the_ship_count() {
        let mut r = Reader::new_row(json!("id"), "generator", "ContractGenerator.X");
        assert_eq!(summary_row(&r)["ships"], json!(0));
        r["ship_n"] = json!(7);
        assert_eq!(summary_row(&r)["ships"], json!(7));
    }
}
