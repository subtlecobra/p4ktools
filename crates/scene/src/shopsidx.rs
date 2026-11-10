//! The shop index as one JSON document (readers and linking are in `shops.rs`): shops grouped into locations,
//! the system -> planet -> moon tree (`lochier`), the inventories with prices, and the item catalogue behind them.
//!
//! Classification helpers are pure and unit tested; `build` needs the archive and is covered by real-archive tests
//! (which skip without the archive).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use p4k::P4k;
use serde_json::{json, Value};

use crate::itemmaterials::ItemMaterials;
use crate::itemstats::ItemStats;
use crate::shops::{link, read_inventories, scan, Kiosk, Shop, Via};

/// Kind of a terminal from its entity class name (INFERRED from the names: only 87 of 491 kiosks carry a provider component themselves).
pub fn kiosk_kind(k: &Kiosk) -> &'static str {
    let c = k.cls.to_lowercase();
    if k.vehicle_usage.as_deref().is_some_and(|u| u.contains("Rent")) {
        "rental"
    } else if c.contains("commoditykiosk") {
        "commodity"
    } else if c.contains("miningkiosk") {
        "mining"
    } else if c.contains("shoppingkiosk") || c == "shopkiosk" {
        "item"
    } else if c.contains("deposit") {
        "deposit"
    } else {
        "other"
    }
}

/// Kind label of a shop from the folder of its container: `ObjectContainers/PU/Shops/<kind>/...` gives the folder name; shops embedded in a
/// station / city layout (`PU/loc/...`) give "embedded in a location".  INFERRED (authoring folder names).
pub fn shop_kind(container: &str) -> String {
    let l = container.to_lowercase();
    let seg: Vec<&str> = l.split('/').collect();
    match seg.iter().position(|s| *s == "shops") {
        Some(i) if seg.len() > i + 2 => seg[i + 1].replace('_', " "),
        _ => "embedded in a location".to_string(),
    }
}

/// Readable shop title from the entity name when there is no localised franchise name: `SCShop_Levski_Dealership_Teachs` -> `Levski Dealership Teachs`.
pub fn shop_title(name: &str) -> String {
    let n = name.strip_prefix("SCShop_").or_else(|| name.strip_prefix("scshop_")).unwrap_or(name);
    // a numbered duplicate `-001` loses its number; digits that belong to the name (`Area18`) stay
    let n = match n.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => n,
    };
    let n = if n.is_empty() { name } else { n };
    n.split('_').filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ")
}

/// `(category, url)` of an item from the record folder (below `records/entities/`) and the `AttachDef.Type`.  Vehicles link the ship zone
/// viewer, personal weapons /weapons/, wearables /gear/, everything else the DataCore record (same rule as the loot and contract pages).
pub fn classify(folder: &str, ty: &str, cls: &str, itemstats_category: &str) -> (&'static str, String) {
    let dcb = || format!("/dcb/record/EntityClassDefinition.{}", cls);
    if folder.starts_with("spaceships") || folder.starts_with("groundvehicles") {
        return ("Ships and vehicles", format!("/ship/{}", cls));
    }
    if folder.starts_with("commodities") {
        return ("Commodities", dcb());
    }
    if ty.starts_with("Char_Armor") {
        return ("Armor", format!("/gear/{}", cls));
    }
    if ty.starts_with("Char_Clothing") {
        return ("Clothing", format!("/gear/{}", cls));
    }
    let cat = match ty {
        "WeaponPersonal" => return ("Personal weapons", format!("/weapons/{}", cls)),
        "WeaponAttachment" => "Weapon attachments",
        "WeaponGun" | "Turret" | "TurretBase" => "Ship weapons",
        "Missile" | "MissileLauncher" | "Bomb" | "BombLauncher" => "Missiles and launchers",
        "Shield" => "Shields",
        "PowerPlant" => "Power plants",
        "Cooler" => "Coolers",
        "QuantumDrive" | "JumpDrive" => "Quantum drives",
        _ if itemstats_category != "other" => "Other ship components",
        _ => "Other items",
    };
    (cat, dcb())
}

/// Breadcrumbs of every location in a `lochier::build_tree` result: `{location index: [["Stanton", "Hurston", ...]]}`, one trail per place the
/// location is listed under.  Unplaced locations are not in the map.
pub fn loc_paths(tree: &Value) -> BTreeMap<usize, Vec<Vec<String>>> {
    fn walk(n: &Value, trail: &mut Vec<String>, out: &mut BTreeMap<usize, Vec<Vec<String>>>) {
        trail.push(n["label"].as_str().unwrap_or("").to_string());
        for l in n["locs"].as_array().into_iter().flatten().filter_map(Value::as_u64) {
            let v = out.entry(l as usize).or_default();
            if !v.contains(trail) {
                v.push(trail.clone());
            }
        }
        for c in n["children"].as_array().into_iter().flatten() {
            walk(c, trail, out);
        }
        trail.pop();
    }
    let mut out = BTreeMap::new();
    for s in tree["systems"].as_array().into_iter().flatten() {
        walk(s, &mut vec![], &mut out);
    }
    out
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Compact display of the rack ports of a shop: `[["Drink/Can", 3], ...]` (size ranges dropped, most frequent first).
pub fn rack_summary(shop: &Shop) -> Vec<Value> {
    let mut m: BTreeMap<String, usize> = BTreeMap::new();
    for p in shop.racks.iter().flat_map(|r| &r.ports) {
        let t = p.rsplit_once(" S").map_or(p.as_str(), |x| x.0);
        *m.entry(t.to_string()).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(t, n)| json!([t, n])).collect()
}

/// An inventory file name as a label: `Inv_TeachsShipShop_Levski` -> `Teachs Ship Shop Levski`.
pub fn inv_label(file: &str) -> String {
    let f = file.strip_prefix("Inv_").unwrap_or(file);
    let mut out = String::new();
    let mut prev_lower = false;
    for c in f.chars() {
        if c == '_' {
            out.push(' ');
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_lowercase();
        out.push(c);
    }
    out
}

/// Last path element of a record name `Type.name`.
fn short(n: &str) -> String {
    n.rsplit_once('.').map_or(n, |x| x.1).to_string()
}

/// The whole index as JSON (cached by the web layer):
/// `{v, locations: [{id, name, title, real_name, dir, path, shops: [shop id]}], tree, shops, inventories, items, stats}`.
/// Shops: `{id, name, title, localised, loc, kind, inv_type, kiosks: [{cls, kind, brand, usage}], racks: [[port, n]], inv, via}`;
/// inventories: `{id, file, label, event, shops: [shop id], via, rows: [[item, buy, sell, cur, max]], unresolved}`; items: `{cls, name, type, cat, mfr, url}`.
pub fn build(items: &ItemMaterials, stats: &ItemStats, p4k: &P4k) -> Value {
    let Some(d) = items.datacore() else {
        return json!({"v": 1, "locations": [], "shops": [], "inventories": [], "items": [], "tree": {"systems": [], "unplaced": []}, "stats": {}});
    };
    let shops = scan(p4k);
    let invs = read_inventories(p4k);
    let links = link(&shops, &invs);

    // locations = the distinct containers of the shops
    let mut paths: Vec<String> = shops.iter().map(|s| s.container.clone()).collect();
    paths.sort();
    paths.dedup();
    let loc_of: HashMap<&str, usize> = paths.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let bodies = crate::lochier::read_bodies(items, stats, p4k);
    let (known, systems) = crate::lochier::known_sets(&bodies);
    let parents = crate::lochier::parent_map(p4k);
    let place = crate::lochier::placements(&paths, &parents, &known, &systems);
    let tree = crate::lochier::build_tree(&bodies, &place);

    // item catalogue of the inventories
    let mut item_idx: HashMap<String, usize> = HashMap::new();
    let mut item_list: Vec<Value> = vec![];
    let mut unresolved = BTreeSet::new();
    let mut cat_counts: BTreeMap<String, usize> = BTreeMap::new();
    for inv in &invs {
        for e in &inv.entries {
            if item_idx.contains_key(&e.id) || unresolved.contains(&e.id) {
                continue;
            }
            let Some(r) = items.guid_record(&e.id) else {
                unresolved.insert(e.id.clone());
                continue;
            };
            let name = d.record_name(r).map(|c| c.to_string()).unwrap_or_default();
            let cls = short(&name);
            let file = d.record_file(r).map(|c| c.to_string()).unwrap_or_default();
            let folder = file.split("records/entities/").nth(1).unwrap_or("").to_string();
            let id = stats.identity(&cls);
            let ty = id["type"].as_str().unwrap_or("");
            let (cat, url) = classify(&folder, ty, &cls, id["category"].as_str().unwrap_or("other"));
            *cat_counts.entry(cat.to_string()).or_default() += 1;
            item_idx.insert(e.id.clone(), item_list.len());
            item_list.push(json!({"cls": cls, "name": id["name"], "type": ty, "cat": cat, "mfr": id["manufacturer"], "url": url}));
        }
    }

    // franchise names (ShopFranchise.localizedName)
    let mut fr_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut fr_name = |g: &str| -> Option<String> {
        fr_cache
            .entry(g.to_string())
            .or_insert_with(|| {
                let r = items.guid_record(g)?;
                let rec = d.record(r).ok()?;
                let v = d.record_data(&rec, 1).ok()?.to_json();
                stats.localise_str(v["localizedName"].as_str()?)
            })
            .clone()
    };

    let mut shop_inv: HashMap<usize, (usize, Via)> = HashMap::new();
    let mut inv_shops: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ii, si, via) in &links {
        shop_inv.entry(*si).or_insert((*ii, *via));
        inv_shops.entry(*ii).or_default().push(*si);
    }
    let mut loc_shops: Vec<Vec<usize>> = vec![vec![]; paths.len()];
    let mut shop_json = vec![];
    let franchises: Vec<Option<String>> = shops.iter().map(|s| s.franchise.as_deref().and_then(&mut fr_name)).collect();
    // shops of one franchise in one container (Teach's has a dealership, an items and a rentals shop) are told apart by their entity names
    let mut same: HashMap<(usize, String), usize> = HashMap::new();
    for (i, s) in shops.iter().enumerate() {
        if let Some(f) = &franchises[i] {
            *same.entry((loc_of[s.container.as_str()], f.clone())).or_default() += 1;
        }
    }
    for (i, s) in shops.iter().enumerate() {
        let li = loc_of[s.container.as_str()];
        loc_shops[li].push(i);
        let franchise = franchises[i].clone();
        let title = match &franchise {
            Some(f) if same[&(li, f.clone())] > 1 => format!("{} ({})", f, shop_title(&s.name)),
            Some(f) => f.clone(),
            None => shop_title(&s.name),
        };
        let kiosks: Vec<Value> = s
            .kiosks
            .iter()
            .map(|k| {
                let brand = k.brand.as_deref().and_then(|b| items.guid_record(b)).and_then(|r| d.record_name(r).ok().map(|n| short(&n)));
                json!({"cls": k.cls, "kind": kiosk_kind(k), "brand": brand, "usage": k.vehicle_usage})
            })
            .collect();
        let (inv, via) = shop_inv.get(&i).map_or((Value::Null, Value::Null), |(ii, v)| (json!(ii), json!(if *v == Via::Guid { "guid" } else { "name" })));
        shop_json.push(json!({"id": i, "name": s.name, "title": title, "localised": franchise.is_some(), "loc": li, "kind": shop_kind(&s.container),
                              "inv_type": s.inv_type, "kiosks": kiosks, "racks": rack_summary(s), "inv": inv, "via": via}));
    }

    let (mut priced_rows, mut rows_total) = (0usize, 0usize);
    let mut inv_json = vec![];
    for (ii, inv) in invs.iter().enumerate() {
        let rows: Vec<Value> = inv
            .entries
            .iter()
            .filter_map(|e| {
                rows_total += 1;
                let it = *item_idx.get(&e.id)?;
                priced_rows += (e.buy > 0.0 || e.sell > 0.0) as usize;
                Some(json!([it, round2(e.buy), round2(e.sell), round2(e.cur), round2(e.max)]))
            })
            .collect();
        let event = inv.file.to_lowercase().contains("anniversarysale");
        let via = links.iter().find(|l| l.0 == ii).map(|l| if l.2 == Via::Guid { "guid" } else { "name" });
        inv_json.push(json!({"id": ii, "file": inv.file, "label": inv_label(&inv.file), "event": event, "shops": inv_shops.get(&ii).cloned().unwrap_or_default(),
                             "via": via, "unresolved": inv.entries.len() - rows.len(), "rows": rows}));
    }

    let loc_json: Vec<Value> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = p.rsplit('/').next().unwrap_or(p).trim_end_matches(".socpak").to_string();
            let dir = p.rsplit_once('/').map_or("", |x| x.0).trim_start_matches("Data/ObjectContainers/").to_string();
            let (title, real) = crate::placename::readable(&name, |k| stats.localise_str(&format!("@{}", k)));
            json!({"id": i, "name": name, "title": title, "real_name": real, "dir": dir, "path": p, "shops": loc_shops[i]})
        })
        .collect();

    let placed_loc = |li: usize| !place[li].0.is_empty() || place[li].1.is_some();
    let linked_shops: HashSet<usize> = links.iter().map(|l| l.1).collect();
    let linked_inv: HashSet<usize> = links.iter().map(|l| l.0).collect();
    let shops_placed: usize = (0..paths.len()).filter(|&li| placed_loc(li)).map(|li| loc_shops[li].len()).sum();
    // inventories whose ShopID names a shop but that were not tied to it (the id is shared with other files: ambiguous)
    let guids: HashSet<&str> = shops.iter().map(|s| s.guid.as_str()).collect();
    let ambiguous = invs.iter().enumerate().filter(|(i, v)| !linked_inv.contains(i) && v.shop_ids.iter().any(|g| guids.contains(g.as_str()))).count();
    let mut kiosk_kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for k in shops.iter().flat_map(|s| &s.kiosks) {
        *kiosk_kinds.entry(kiosk_kind(k)).or_default() += 1;
    }
    let stats_j = json!({
        "shops": shops.len(), "shops_placed": shops_placed, "shops_unplaced": shops.len() - shops_placed,
        "locations": paths.len(), "locations_placed": (0..paths.len()).filter(|&li| placed_loc(li)).count(),
        "shops_with_inventory": linked_shops.len(), "shops_without_inventory": shops.len() - linked_shops.len(),
        "shops_with_kiosk": shops.iter().filter(|s| !s.kiosks.is_empty()).count(), "shops_with_racks": shops.iter().filter(|s| !s.racks.is_empty()).count(),
        "kiosks": shops.iter().map(|s| s.kiosks.len()).sum::<usize>(), "kiosk_kinds": kiosk_kinds,
        "inventories": invs.len(), "inventories_linked": linked_inv.len(), "inventories_ambiguous": ambiguous, "inventories_event": inv_json.iter().filter(|v| v["event"] == true).count(),
        "links_guid": links.iter().filter(|l| l.2 == Via::Guid).count(), "links_name": links.iter().filter(|l| l.2 == Via::Name).count(),
        "rows": rows_total, "rows_priced": priced_rows,
        "items": item_list.len(), "items_unresolved": unresolved.len(), "items_by_category": cat_counts,
        "ships_priced": item_list.iter().filter(|i| i["cat"] == "Ships and vehicles").count(),
        "rental_rows": invs.iter().flat_map(|i| &i.entries).filter(|e| e.rentals > 0).count(),
    });
    json!({"v": 1, "locations": loc_json, "tree": tree, "shops": shop_json, "inventories": inv_json, "items": item_list, "stats": stats_j})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shops::Rack;

    #[test]
    fn classifies_items_and_titles() {
        assert_eq!(classify("spaceships/x.xml", "NOITEM_Vehicle", "AEGS_Gladius", "other"), ("Ships and vehicles", "/ship/AEGS_Gladius".to_string()));
        assert_eq!(classify("commodities/metals/gold.xml", "", "Gold", "other").0, "Commodities");
        assert_eq!(classify("scitem/characters/x.xml", "Char_Armor_Helmet", "h", "other"), ("Armor", "/gear/h".to_string()));
        assert_eq!(classify("scitem/weapons/x.xml", "WeaponPersonal", "w", "weapons").1, "/weapons/w");
        assert_eq!(classify("scitem/ships/x.xml", "Cooler", "c", "coolers"), ("Coolers", "/dcb/record/EntityClassDefinition.c".to_string()));
        assert_eq!(classify("scitem/ships/x.xml", "Radar", "r", "sensors").0, "Other ship components");
        assert_eq!(classify("scitem/x.xml", "Misc", "m", "other").0, "Other items");
        assert_eq!(shop_title("SCShop_Levski_Dealership_Teachs"), "Levski Dealership Teachs");
        assert_eq!(shop_title("SCShop_Levski-001"), "Levski");
        assert_eq!(shop_title("SCShop_AstroArmada_Area18"), "AstroArmada Area18");
        assert_eq!(shop_title("TDD_SCShop-001"), "TDD SCShop");
        assert_eq!(shop_title("SCShop_Admin_levski-001"), "Admin levski");
        assert_eq!(shop_kind("Data/ObjectContainers/PU/Shops/armor/cordrys/cordrys_levski_b.socpak"), "armor");
        assert_eq!(shop_kind("Data/ObjectContainers/PU/loc/flagship/nyx/levski/levski_int.socpak"), "embedded in a location");
        assert_eq!(inv_label("Inv_TeachsShipShop_Levski"), "Teachs Ship Shop Levski");
    }

    #[test]
    fn kiosk_kinds_by_class_name() {
        let k = |cls: &str, u: Option<&str>| Kiosk { cls: cls.into(), vehicle_usage: u.map(String::from), ..Default::default() };
        assert_eq!(kiosk_kind(&k("ShopKiosk", Some("Rent"))), "rental");
        assert_eq!(kiosk_kind(&k("SoftLock_Terminal_Standard_LowTech_CommodityKiosk_kiosk_tdd_1_a", None)), "commodity");
        assert_eq!(kiosk_kind(&k("SoftLock_Terminal_Standard_HighTech_ShoppingKiosk_newbab_1_a", None)), "item");
        assert_eq!(kiosk_kind(&k("MASTER_screen_16x9_MiningKiosk", None)), "mining");
        assert_eq!(kiosk_kind(&k("Whatever", None)), "other");
    }

    #[test]
    fn breadcrumbs_from_a_tree() {
        let t = json!({"systems": [{"label": "Stanton", "locs": [], "children": [
            {"label": "Hurston", "locs": [1], "children": [{"label": "Aberdeen", "locs": [0, 1], "children": []}]}]}], "unplaced": [2]});
        let p = loc_paths(&t);
        assert_eq!(p[&0], vec![vec!["Stanton".to_string(), "Hurston".into(), "Aberdeen".into()]]);
        assert_eq!(p[&1].len(), 2);
        assert!(!p.contains_key(&2));
    }

    #[test]
    fn rack_summary_counts_ports() {
        let s = Shop { racks: vec![Rack { guid: "r".into(), ports: vec!["Drink/Can S1-1".into(), "Drink/Can S1-2".into(), "Cooler S1-3".into()] }], ..Default::default() };
        assert_eq!(rack_summary(&s), vec![json!(["Drink/Can", 2]), json!(["Cooler", 1])]);
    }
}
