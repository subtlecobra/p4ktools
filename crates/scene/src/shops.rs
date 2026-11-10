//! Shop index: shops (`SCShop` entities), their kiosks and racks, and the static inventories of `Data/Scripts/ShopInventories/*.json`.
//! This file only has the readers.
//!
//! What is read (EMPIRICAL, live 4.x archive):
//! * A **shop** is an entity of class `SCShop` in a container (`.soc` static entities, also `entdata/*.entxml`).  Its components:
//!   `EntityComponentShop@franchise` (GUID of a `ShopFranchise` record, `localizedName` = shop name key), `@shopInventoryType`, and the lists
//!   `ShopRackGUIDs/ShopRackGUID@entityCryGUID`, `ShopKioskGUIDs/ShopKioskGUID@entityCryGUID` (EntityCryGUIDs of entities in the same container).
//! * A **kiosk** (class `ShopKiosk`, `SoftLock_Terminal_*ShoppingKiosk*` ...) is the terminal entity; `EntityComponentShoppingProvider@brand`
//!   (an `ItemKioskBrand` record) and `EntityComponentPurchasableVehicleProvider@vehicleUsageTypes` (`Rent` / `Buy` ...) say what it offers.
//! * A **rack** is a display entity whose `ItemPortContainer` ports name the item type it can show (`SItemPortDefTypes@Type` + sub types).
//! * An **inventory** file lists `{ID: [entity class GUID], BuyPrice, SellPrice, CurrentInventory, MaxInventory, RentalOfferings}` and (newer
//!   files) the `ShopID` string: a comma separated list of `SCShop` EntityCryGUIDs that use it.
//!
//! Nothing here is CONFIRMED against the engine: the field meanings follow from names and from cross checks on the data (a `ShopID` GUID
//! found as an `SCShop` entity, an item GUID found as an `EntityClassDefinition` record).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use p4k_formats::cryxml::{self, Element};
use p4k_formats::socpak::{iter_all, SocZip};
use p4k::P4k;
use serde_json::{json, Value};

/// One terminal (kiosk) entity of a shop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Kiosk {
    pub guid: String,
    pub name: String,
    pub cls: String,
    /// `ItemKioskBrand` record GUID (`EntityComponentShoppingProvider@brand`)
    pub brand: Option<String>,
    /// `EntityComponentPurchasableVehicleProvider@vehicleUsageTypes` (`Rent`, `Buy` ...)
    pub vehicle_usage: Option<String>,
    /// component tags of the entity that look like providers (`EntityComponent*Provider*`), for kinds this reader does not know
    pub providers: Vec<String>,
}

/// One display rack of a shop: the item types its ports accept (`Type/SubType`, with size range).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rack {
    pub guid: String,
    pub ports: Vec<String>,
}

/// One `SCShop` entity.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shop {
    /// archive path of the container
    pub container: String,
    pub name: String,
    pub guid: String,
    pub franchise: Option<String>,
    pub inv_type: String,
    pub pos: [f64; 3],
    pub kiosks: Vec<Kiosk>,
    pub racks: Vec<Rack>,
    /// number of rack / kiosk GUIDs the shop lists that were not found in the container
    pub missing: usize,
}

/// `[x, y, z]` of a `Pos` attribute.
fn pos3(s: Option<&str>) -> [f64; 3] {
    let v: Vec<f64> = s.unwrap_or("").split(',').filter_map(|t| t.trim().parse().ok()).collect();
    if v.len() == 3 {
        [v[0], v[1], v[2]]
    } else {
        [0.0; 3]
    }
}

fn guid_list(root: &Element, list_tag: &str, item_tag: &str) -> Vec<String> {
    let mut v = vec![];
    for l in iter_all(root).into_iter().filter(|e| e.tag == list_tag) {
        for c in l.children.iter().filter(|c| c.tag == item_tag) {
            if let Some(g) = c.get("entityCryGUID") {
                v.push(g.to_string());
            }
        }
    }
    v
}

/// The `SCShop` header of an entity element: `(shop with empty kiosks / racks, kiosk guids, rack guids)`.
pub fn shop_of(container: &str, el: &Element) -> Option<(Shop, Vec<String>, Vec<String>)> {
    if el.tag != "Entity" || el.get("EntityClass") != Some("SCShop") {
        return None;
    }
    let comp = iter_all(el).into_iter().find(|e| e.tag == "EntityComponentShop" && e.get("franchise").is_some());
    let shop = Shop {
        container: container.to_string(),
        name: el.get("Name").unwrap_or("").to_string(),
        guid: el.get("EntityCryGUID")?.to_string(),
        franchise: comp.and_then(|c| c.get("franchise")).filter(|s| !s.is_empty()).map(str::to_string),
        inv_type: comp.and_then(|c| c.get("shopInventoryType")).unwrap_or("").to_string(),
        pos: pos3(el.get("Pos")),
        ..Default::default()
    };
    Some((shop, guid_list(el, "ShopKioskGUIDs", "ShopKioskGUID"), guid_list(el, "ShopRackGUIDs", "ShopRackGUID")))
}

/// A kiosk entity.
pub fn kiosk_of(el: &Element) -> Kiosk {
    let all = iter_all(el);
    let attr = |tag: &str, key: &str| all.iter().find(|e| e.tag == tag).and_then(|e| e.get(key)).filter(|s| !s.is_empty()).map(str::to_string);
    let mut providers: Vec<String> = all.iter().filter(|e| e.tag.starts_with("EntityComponent") && e.tag.contains("Provider")).map(|e| e.tag.clone()).collect();
    providers.dedup();
    Kiosk {
        guid: el.get("EntityCryGUID").unwrap_or("").to_string(),
        name: el.get("Name").unwrap_or("").to_string(),
        cls: el.get("EntityClass").unwrap_or("").to_string(),
        brand: attr("EntityComponentShoppingProvider", "brand"),
        vehicle_usage: attr("EntityComponentPurchasableVehicleProvider", "vehicleUsageTypes"),
        providers,
    }
}

/// A rack entity: one string per port, `Type[/SubType,SubType] size min-max`.
pub fn rack_of(el: &Element) -> Rack {
    let mut ports = vec![];
    for p in iter_all(el).into_iter().filter(|e| e.tag == "SItemPortDef") {
        let size = format!("S{}-{}", p.get("MinSize").unwrap_or("?"), p.get("MaxSize").unwrap_or("?"));
        for t in iter_all(p).into_iter().filter(|e| e.tag == "SItemPortDefTypes") {
            let subs: Vec<&str> = iter_all(t).into_iter().filter(|e| e.tag == "enum").filter_map(|e| e.get("value")).collect();
            let ty = t.get("Type").unwrap_or("?");
            ports.push(if subs.is_empty() { format!("{} {}", ty, size) } else { format!("{}/{} {}", ty, subs.join(","), size) });
        }
    }
    Rack { guid: el.get("EntityCryGUID").unwrap_or("").to_string(), ports }
}

/// Shops of one parsed entity document (a `.soc` `<Entities>` root or one `entdata` entity), with kiosks and racks filled from the same document.
pub fn shops_in(container: &str, root: &Element) -> Vec<Shop> {
    let all = iter_all(root);
    let heads: Vec<(Shop, Vec<String>, Vec<String>)> = all.iter().filter_map(|e| shop_of(container, e)).collect();
    if heads.is_empty() {
        return vec![];
    }
    let want: HashSet<&str> = heads.iter().flat_map(|h| h.1.iter().chain(h.2.iter())).map(String::as_str).collect();
    // an <Entity> carrying only a GUID repeats a full record elsewhere in the document; keep the one with content
    let mut by_guid: HashMap<&str, &Element> = HashMap::new();
    for e in all.iter().filter(|e| e.tag == "Entity") {
        if let Some(g) = e.get("EntityCryGUID").filter(|g| want.contains(g)) {
            let keep = by_guid.get(g).map_or(true, |o| e.attrs.len() + e.children.len() > o.attrs.len() + o.children.len());
            if keep {
                by_guid.insert(g, e);
            }
        }
    }
    heads
        .into_iter()
        .map(|(mut s, kiosks, racks)| {
            for g in &kiosks {
                match by_guid.get(g.as_str()) {
                    Some(e) => s.kiosks.push(kiosk_of(e)),
                    None => s.missing += 1,
                }
            }
            for g in &racks {
                match by_guid.get(g.as_str()) {
                    Some(e) => s.racks.push(rack_of(e)),
                    None => s.missing += 1,
                }
            }
            s
        })
        .collect()
}

/// Every shop of every container of the archive (reads each container once; a `.soc` / entity file is only parsed when it mentions `SCShop`).
pub fn scan(p4k: &P4k) -> Vec<Shop> {
    let mut out = vec![];
    for e in p4k.entries.iter().filter(|e| e.name.to_lowercase().ends_with(".socpak") && e.name.contains("ObjectContainers")) {
        let Ok(data) = p4k.read(e) else { continue };
        let Ok(z) = SocZip::open(&data) else { continue };
        let names: Vec<String> = z.namelist().filter(|n| n.ends_with(".soc") || (n.ends_with(".entxml") && n.contains("entdata"))).map(String::from).collect();
        for n in names {
            let Ok(b) = z.read(&n) else { continue };
            if memchr::memmem::find(&b, b"SCShop").is_none() {
                continue;
            }
            let Some(o) = memchr::memmem::find(&b, b"CryXmlB") else { continue };
            if let Ok(root) = cryxml::parse(&b[o..]) {
                out.extend(shops_in(&e.name, &root));
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------------------------- inventories

/// One priced entry of an inventory file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    /// EntityClassDefinition record GUID
    pub id: String,
    pub buy: f64,
    pub sell: f64,
    pub cur: f64,
    pub max: f64,
    /// number of `RentalOfferings`
    pub rentals: usize,
    /// raw rental offerings (shape not verified), kept for display
    pub rental: Vec<Value>,
}

/// A parsed `Inv_*.json`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inventory {
    /// archive file name without the folder and extension (`Inv_NewDeal_Lorville`)
    pub file: String,
    /// `SCShop` EntityCryGUIDs listed in `ShopID` (empty for files without it)
    pub shop_ids: Vec<String>,
    pub entries: Vec<Entry>,
}

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Parses an inventory document: either `{ShopID, Collection: {Inventory}}` or `{Inventory}`.
pub fn parse_inventory(file: &str, data: &[u8]) -> Option<Inventory> {
    let v: Value = serde_json::from_slice(data).ok()?;
    let inv = v.get("Collection").and_then(|c| c.get("Inventory")).or_else(|| v.get("Inventory"))?.as_array()?;
    let shop_ids = v.get("ShopID").and_then(Value::as_str).unwrap_or("").split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    let entries = inv
        .iter()
        .filter_map(|e| {
            let id = e.get("ID")?.get("ID")?.as_array()?.first()?.as_str()?.to_lowercase();
            let rental = e.get("RentalOfferings").and_then(Value::as_array).cloned().unwrap_or_default();
            Some(Entry { id, buy: num(e, "BuyPrice"), sell: num(e, "SellPrice"), cur: num(e, "CurrentInventory"), max: num(e, "MaxInventory"), rentals: rental.len(), rental })
        })
        .collect();
    Some(Inventory { file: file.to_string(), shop_ids, entries })
}

/// All `Data/Scripts/ShopInventories/*.json` of the archive.
pub fn read_inventories(p4k: &P4k) -> Vec<Inventory> {
    let mut out = vec![];
    for e in p4k.entries.iter().filter(|e| {
        let n = e.name.to_lowercase();
        n.starts_with("data/scripts/shopinventories/") && n.ends_with(".json")
    }) {
        let Ok(d) = p4k.read(e) else { continue };
        let stem = e.name.rsplit('/').next().unwrap_or(&e.name).trim_end_matches(".json").to_string();
        if let Some(i) = parse_inventory(&stem, &d) {
            out.push(i);
        }
    }
    out
}

/// Order-insensitive name key of a shop entity (`SCShop_Mining_Levski`) or an inventory file (`Inv_Mining_Levski`): lower-case alphanumeric tokens
/// split at every other character, without the `scshop` / `inv` / `entity` markers and numbers (`-001`), sorted and joined.  Case changes do not split
/// (`GrimHex` and `Grimhex` must agree).  INFERRED: the two names are authored by hand in different places; checked on the data,
/// where the equal keys are the same place every time.
pub fn name_key(name: &str) -> String {
    let mut toks: Vec<String> = name.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(String::from).collect();
    toks.retain(|t| !matches!(t.as_str(), "scshop" | "inv" | "entity") && !t.chars().all(|c| c.is_ascii_digit()));
    toks.sort();
    toks.dedup();
    toks.join("_")
}

/// How an inventory was tied to a shop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Via {
    /// the inventory's `ShopID` list holds the shop's EntityCryGUID and no other inventory without a name match claims the shop
    Guid,
    /// the name keys are equal ([`name_key`])
    Name,
}

/// Ties inventories to shops: `(inventory index, shop index, how)`.
///
/// EMPIRICAL: `ShopID` is copied between files by the authors (the New Deal, Astro Armada and Regal Luxury files all carry
/// New Deal's chain; a dozen food files carry one Port Olisar shop), so a GUID alone is not trusted when it is shared.  Rules, in order:
/// 1. an inventory whose name key equals a shop's name key is tied to every shop with that key (`Via::Name`);
/// 2. an inventory with no name match is tied by its `ShopID` to a shop only when it is the single such inventory claiming that shop and no
///    name-matched inventory claims the shop (`Via::Guid`); a shop claimed by several unnamed inventories stays without one (ambiguous).
pub fn link(shops: &[Shop], invs: &[Inventory]) -> Vec<(usize, usize, Via)> {
    let by_guid: HashMap<&str, usize> = shops.iter().enumerate().map(|(i, s)| (s.guid.as_str(), i)).collect();
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, s) in shops.iter().enumerate() {
        by_name.entry(name_key(&s.name)).or_default().push(i);
    }
    let mut out = vec![];
    let mut named_shops: HashSet<usize> = HashSet::new();
    let mut unnamed: Vec<usize> = vec![];
    for (ii, inv) in invs.iter().enumerate() {
        let key = name_key(&inv.file);
        match by_name.get(&key).filter(|_| !key.is_empty()) {
            Some(v) => {
                for &si in v {
                    out.push((ii, si, Via::Name));
                    named_shops.insert(si);
                }
            }
            None => unnamed.push(ii),
        }
    }
    let mut claims: HashMap<usize, Vec<usize>> = HashMap::new();
    for &ii in &unnamed {
        for g in &invs[ii].shop_ids {
            if let Some(&si) = by_guid.get(g.as_str()) {
                let c = claims.entry(si).or_default();
                if !c.contains(&ii) {
                    c.push(ii);
                }
            }
        }
    }
    let mut guid_links: Vec<(usize, usize, Via)> = claims.into_iter().filter(|(si, c)| c.len() == 1 && !named_shops.contains(si)).map(|(si, c)| (c[0], si, Via::Guid)).collect();
    guid_links.sort();
    out.extend(guid_links);
    out
}

/// Summary used by the doc and the tests: shops per inventory type, kiosk classes, usage types.
pub fn summary(shops: &[Shop]) -> Value {
    let mut types: BTreeMap<String, usize> = BTreeMap::new();
    let mut kcls: BTreeMap<String, usize> = BTreeMap::new();
    let mut usage: BTreeMap<String, usize> = BTreeMap::new();
    let mut containers = BTreeSet::new();
    for s in shops {
        *types.entry(s.inv_type.clone()).or_default() += 1;
        containers.insert(s.container.clone());
        for k in &s.kiosks {
            *kcls.entry(k.cls.clone()).or_default() += 1;
            *usage.entry(k.vehicle_usage.clone().unwrap_or_default()).or_default() += 1;
        }
    }
    json!({"shops": shops.len(), "containers": containers.len(), "inventory_types": types, "kiosk_classes": kcls, "vehicle_usage": usage})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(tag: &str, attrs: &[(&str, &str)], children: Vec<Element>) -> Element {
        Element { tag: tag.into(), attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), text: String::new(), children }
    }

    fn doc() -> Element {
        let shop = el(
            "Entity",
            &[("Name", "SCShop_X"), ("EntityClass", "SCShop"), ("EntityCryGUID", "s1"), ("Pos", "1,2,3")],
            vec![
                el("PropertiesDataCore", &[], vec![el("EntityComponentShop", &[("franchise", "f1"), ("shopInventoryType", "ITEM")], vec![])]),
                el(
                    "EntityComponentShop",
                    &[],
                    vec![
                        el("ShopRackGUIDs", &[], vec![el("ShopRackGUID", &[("entityCryGUID", "r1")], vec![])]),
                        el("ShopKioskGUIDs", &[], vec![el("ShopKioskGUID", &[("entityCryGUID", "k1")], vec![]), el("ShopKioskGUID", &[("entityCryGUID", "k2")], vec![])]),
                    ],
                ),
            ],
        );
        let kiosk = el(
            "Entity",
            &[("Name", "ShopKiosk-1"), ("EntityClass", "ShopKiosk"), ("EntityCryGUID", "k1")],
            vec![
                el("EntityComponentPurchasableVehicleProvider", &[("vehicleUsageTypes", "Rent")], vec![]),
                el("EntityComponentShoppingProvider", &[("brand", "b1")], vec![]),
            ],
        );
        let bare = el("Entity", &[("EntityCryGUID", "k1")], vec![]);
        let rack = el(
            "Entity",
            &[("Name", "rack"), ("EntityClass", "display"), ("EntityCryGUID", "r1")],
            vec![el(
                "SItemPortDef",
                &[("MinSize", "2"), ("MaxSize", "3")],
                vec![el("SItemPortDefTypes", &[("Type", "WeaponGun")], vec![el("SubTypes", &[], vec![el("enum", &[("value", "Gun")], vec![])])])],
            )],
        );
        el("Entities", &[], vec![shop, bare, kiosk, rack, el("Entity", &[("EntityClass", "Other"), ("EntityCryGUID", "z")], vec![])])
    }

    #[test]
    fn reads_shop_kiosks_and_racks() {
        let shops = shops_in("Data/ObjectContainers/a.socpak", &doc());
        assert_eq!(shops.len(), 1);
        let s = &shops[0];
        assert_eq!((s.guid.as_str(), s.franchise.as_deref(), s.inv_type.as_str(), s.pos), ("s1", Some("f1"), "ITEM", [1.0, 2.0, 3.0]));
        assert_eq!(s.kiosks.len(), 1);
        assert_eq!(s.kiosks[0].cls, "ShopKiosk");
        assert_eq!(s.kiosks[0].brand.as_deref(), Some("b1"));
        assert_eq!(s.kiosks[0].vehicle_usage.as_deref(), Some("Rent"));
        assert_eq!(s.missing, 1, "k2 is not in the document");
        assert_eq!(s.racks[0].ports, vec!["WeaponGun/Gun S2-3".to_string()]);
    }

    #[test]
    fn name_keys_ignore_order_markers_and_numbers() {
        assert_eq!(name_key("SCShop_Mining_Levski"), name_key("Inv_Mining_Levski"));
        assert_eq!(name_key("SCShop_Levski-001"), "levski");
        assert_eq!(name_key("Inv_TeachsShipShop_Levski"), "levski_teachsshipshop");
        assert_eq!(name_key("Levski_Mining"), name_key("SCShop_Mining_Levski"));
        assert_eq!(name_key("Inv_Skutters_Grimhex"), name_key("SCShop_Skutters_GrimHex"));
    }

    #[test]
    fn links_by_name_then_unshared_guid() {
        let sh = |n: &str, g: &str| Shop { name: n.into(), guid: g.into(), ..Default::default() };
        let shops = vec![sh("SCShop_A_X", "g1"), sh("SCShop_B_X", "g2"), sh("SCShop_B_X-002", "g3"), sh("SCShop_C", "g4"), sh("SCShop_D", "g5"), sh("SCShop_E", "g6")];
        let inv = |f: &str, ids: &[&str]| Inventory { file: f.into(), shop_ids: ids.iter().map(|s| s.to_string()).collect(), entries: vec![] };
        let invs = vec![
            inv("Inv_Z", &["root", "g1"]),        // sole unnamed claimant of g1: tied by guid
            inv("Inv_B_X", &["g1"]),              // name match to both B shops; its ShopID is ignored
            inv("Inv_Q", &["nope"]),              // nothing
            inv("Inv_", &[]),                     // empty key
            inv("Inv_Copy1", &["g4", "g5"]),      // g4 and g5 are claimed by two unnamed inventories: ambiguous
            inv("Inv_Copy2", &["g4"]),
            inv("Inv_Own", &["g6"]),
            inv("Inv_E", &["g4"]),                // name matches SCShop_E (g6), not g4
        ];
        let l = link(&shops, &invs);
        assert_eq!(l, vec![(1, 1, Via::Name), (1, 2, Via::Name), (7, 5, Via::Name), (0, 0, Via::Guid), (4, 4, Via::Guid)]);
    }

    #[test]
    fn non_shop_documents_give_nothing() {
        assert!(shops_in("x", &el("Entities", &[], vec![el("Entity", &[("EntityClass", "Other")], vec![])])).is_empty());
    }

    #[test]
    fn inventory_both_shapes() {
        let a = br#"{"ShopID":"AA-1, bb-2","Collection":{"Inventory":[{"ID":{"ID":["G1"]},"BuyPrice":10.5,"SellPrice":0,"CurrentInventory":3,"MaxInventory":9,"RentalOfferings":[{"x":1}]}]}}"#;
        let i = parse_inventory("Inv_A", a).unwrap();
        assert_eq!(i.shop_ids, vec!["aa-1", "bb-2"]);
        assert_eq!((i.entries[0].id.as_str(), i.entries[0].buy, i.entries[0].rentals), ("g1", 10.5, 1));
        let b = br#"{"Inventory":[{"ID":{"ID":["g2"]},"BuyPrice":0,"SellPrice":2}]}"#;
        let j = parse_inventory("Inv_B", b).unwrap();
        assert!(j.shop_ids.is_empty());
        assert_eq!(j.entries[0].sell, 2.0);
        assert!(parse_inventory("bad", b"nope").is_none());
    }
}
