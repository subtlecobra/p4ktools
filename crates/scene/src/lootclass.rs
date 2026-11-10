//! What an entity CLASS brings to a harvestable slot: its own loot config and its fixed (loadout) contents.
//!
//! The slot's `lootConfig` is not the only source.  How the engine fills a slot (CONFIRMED unless marked):
//! * Filling a slot sets the spawn's `LootConfig` property only when the slot's lootConfig has a V3 / legacy table; the entity is spawned either way.
//! * The item port container merges the per-port default loadouts (`SItemPortContainerComponentParams` ports) with the class's
//!   `EntityComponentDefaultLoadout`; the loadout spawn spawns those items, then looks up the entity's loot config: the spawn's `LootConfig`
//!   property first, THEN the class's `LootGenerationComponentParams.lootConfig` (type `LootConfig`).  If a config is found, loot generation
//!   fills the entity's own inventory.
//! * Order in a crate: loadout items, mission items, generated loot on top; the capacity is reduced by what is already inside.
//! * A loot config is invalid when both table instances are null: no generation for THAT config, the class fallback can still supply one.
//! * INFERRED: a crate with only an `SCItemInventoryContainerComponentParams` and no ItemPortContainer is not covered by the port container's
//!   spawn; its loot then comes from the loot component's own generation (no direct caller found: script / server / mission / debug
//!   triggered) or the loadout spawn's inventory-container path when a loadout exists.
//!
//! DataCore names are EMPIRICAL (Game2.dcb schema of the live 4.x archive): component `LootGenerationComponentParams{lootConfig: LootConfig{lootConstraints,
//! lootTable, lootTableV3}}`, `SEntityComponentDefaultLoadoutParams{loadout: SItemPortLoadoutBaseParams}` with the subtypes `SItemPortLoadoutManualParams{entries[]
//! {itemPortName, entityClassName, entityClassReference, inventoryContainer{inventoryItems[]}, loadout}}`, `...RecordParams{loadoutRecord}`,
//! `...XMLParams{loadoutPath}`, `...RandomGeneratedParams`, `...OutfitParams`, `SItemPortActorRecordParams`, all with `InventoryItems[]{entityClassName,
//! entityClassReference, amount}`; ports `SItemPortContainerComponentParams.Ports[].defaultItem{itemPort, entityClass, childItems[]}`.
//!
//! Pure functions over the record JSON (`Dcb::record_data(..).to_json()`); references are resolved through [`Resolve`] so the walkers are testable.

use serde_json::{json, Value};

use crate::dynv::{arr, f, g, s, truthy};

/// Resolves DataCore references for the walkers.
pub trait Resolve {
    /// Entity class name (without the record prefix) of a reference value `["ref", n, guid]`.
    fn class_of(&self, r: &Value) -> Option<String>;
    /// `EntityDefaultLoadoutParams` record data behind a reference.
    fn loadout_record(&self, r: &Value) -> Option<Value>;
    /// Full record name (`LootTable.x`) behind a reference.
    fn record_name(&self, r: &Value) -> Option<String>;
}

/// The class's own loot config (`LootGenerationComponentParams.lootConfig`).
#[derive(Clone, Debug, PartialEq)]
pub struct ClassLoot {
    /// the `LootTable` (V1) record name
    pub table: Option<String>,
    /// the `LootTableV3Record` name
    pub table_v3: Option<String>,
    /// raw `lootConstraints` (the caller decodes them like a slot's)
    pub constraints: Value,
}

/// One item the class always carries.
#[derive(Clone, Debug, PartialEq)]
pub struct FixedItem {
    pub cls: String,
    pub amount: i64,
    pub port: String,
    /// `loadout` (default loadout entry), `loadout inventory` (entry inventory container / `InventoryItems`), `port default` (`SItemPortDef.defaultItem`)
    pub via: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClassFill {
    pub loot: Option<ClassLoot>,
    pub fixed: Vec<FixedItem>,
    /// loadout kinds that were not resolved (`xml:<path>`, `random generated`, `outfit`, `actor record`), so a "no fixed items" claim is not made for them
    pub unresolved: Vec<String>,
    /// the class has an `SCItemInventoryContainerComponentParams` (a crate that can hold generated loot)
    pub inventory: bool,
    /// the class has an `SItemPortContainerComponentParams` with at least one port
    pub ports: bool,
}

const MAX_DEPTH: usize = 6;
const MAX_ITEMS: usize = 300;

fn comp<'a>(rec: &'a Value, name: &str) -> &'a Value {
    arr(g(rec, "Components")).iter().find(|c| truthy(c) && s(g(c, "__struct__")) == name).unwrap_or(&Value::Null)
}

fn ref_is_set(v: &Value) -> bool {
    v.as_array().is_some_and(|a| a.len() == 3 && a[0] == "ref" && a[2].as_str().is_some_and(|g| g != "00000000-0000-0000-0000-000000000000"))
}

/// The class named by an `entityClassName` / `entityClassReference` pair (the name wins when set, as in `ItemMaterials::entry_class`).
fn entry_class(e: &Value, name_key: &str, ref_key: &str, res: &dyn Resolve) -> Option<String> {
    let n = s(g(e, name_key));
    if !n.is_empty() {
        return Some(n.to_string());
    }
    if ref_is_set(g(e, ref_key)) {
        return res.class_of(g(e, ref_key));
    }
    None
}

fn inventory_items(base: &Value, via: &'static str, port: &str, out: &mut ClassFill, res: &dyn Resolve) {
    for it in arr(g(base, "InventoryItems")).iter().chain(arr(g(base, "inventoryItems"))) {
        if let Some(cls) = entry_class(it, "entityClassName", "entityClassReference", res) {
            let amount = if g(it, "amount").is_null() { 1 } else { f(g(it, "amount")) as i64 };
            push(out, FixedItem { cls, amount, port: port.to_string(), via });
        }
    }
}

fn push(out: &mut ClassFill, it: FixedItem) {
    if out.fixed.len() < MAX_ITEMS {
        out.fixed.push(it);
    }
}

fn walk_loadout(base: &Value, depth: usize, out: &mut ClassFill, res: &dyn Resolve) {
    if depth > MAX_DEPTH || !truthy(base) {
        return;
    }
    inventory_items(base, "loadout inventory", "", out, res);
    match s(g(base, "__struct__")) {
        "SItemPortLoadoutManualParams" => {
            for e in arr(g(base, "entries")) {
                let port = s(g(e, "itemPortName"));
                if let Some(cls) = entry_class(e, "entityClassName", "entityClassReference", res) {
                    push(out, FixedItem { cls, amount: 1, port: port.to_string(), via: "loadout" });
                }
                inventory_items(g(e, "inventoryContainer"), "loadout inventory", port, out, res);
                walk_loadout(g(e, "loadout"), depth + 1, out, res);
            }
        }
        "SItemPortLoadoutRecordParams" => {
            if let Some(rec) = res.loadout_record(g(base, "loadoutRecord")) {
                walk_loadout(g(&rec, "loadout"), depth + 1, out, res);
            } else {
                out.unresolved.push("loadout record".into());
            }
        }
        "SItemPortLoadoutXMLParams" => out.unresolved.push(format!("xml:{}", s(g(base, "loadoutPath")))),
        "SItemPortLoadoutRandomGeneratedParams" => out.unresolved.push("random generated".into()),
        "SItemPortLoadoutOutfitParams" => {
            out.unresolved.push("outfit".into());
            walk_loadout(g(base, "BaseLoadout"), depth + 1, out, res);
        }
        "SItemPortActorRecordParams" => out.unresolved.push("actor record".into()),
        _ => {}
    }
}

fn walk_port_default(d: &Value, depth: usize, out: &mut ClassFill, res: &dyn Resolve) {
    if depth > MAX_DEPTH || !truthy(d) {
        return;
    }
    let port = s(g(d, "itemPort"));
    if ref_is_set(g(d, "entityClass")) {
        if let Some(cls) = res.class_of(g(d, "entityClass")) {
            push(out, FixedItem { cls, amount: 1, port: port.to_string(), via: "port default" });
        }
    }
    for c in arr(g(d, "childItems")) {
        walk_port_default(c, depth + 1, out, res);
    }
}

/// Reads the class record (`Components` array).
pub fn read(rec: &Value, res: &dyn Resolve) -> ClassFill {
    let mut out = ClassFill::default();
    let lg = comp(rec, "LootGenerationComponentParams");
    if truthy(lg) {
        let lc = g(lg, "lootConfig");
        let name = |k: &str| -> Option<String> { ref_is_set(g(lc, k)).then(|| res.record_name(g(lc, k))).flatten() };
        let (t1, t3) = (name("lootTable"), name("lootTableV3"));
        if t1.is_some() || t3.is_some() {
            out.loot = Some(ClassLoot { table: t1, table_v3: t3, constraints: g(lc, "lootConstraints").clone() });
        }
    }
    out.inventory = truthy(comp(rec, "SCItemInventoryContainerComponentParams"));
    let dl = comp(rec, "SEntityComponentDefaultLoadoutParams");
    if truthy(dl) {
        walk_loadout(g(dl, "loadout"), 0, &mut out, res);
    }
    let pc = comp(rec, "SItemPortContainerComponentParams");
    let ports = arr(g(pc, "Ports"));
    out.ports = !ports.is_empty();
    for p in ports {
        walk_port_default(g(p, "defaultItem"), 0, &mut out, res);
    }
    out
}

/// JSON form for the index: `{loot: {..}|null, fixed: [{cls, amount, port, via}], unresolved: [..], inventory, ports}`.
pub fn to_json(c: &ClassFill) -> Value {
    json!({
        "loot": c.loot.as_ref().map(|l| json!({"table": l.table, "table_v3": l.table_v3})),
        "fixed": c.fixed.iter().map(|i| json!({"cls": i.cls, "amount": i.amount, "port": i.port, "via": i.via})).collect::<Vec<_>>(),
        "unresolved": c.unresolved, "inventory": c.inventory, "ports": c.ports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct R;
    impl Resolve for R {
        fn class_of(&self, r: &Value) -> Option<String> {
            Some(format!("cls_{}", r[2].as_str()?))
        }
        fn loadout_record(&self, r: &Value) -> Option<Value> {
            (r[2] == "rec1").then(|| json!({"loadout": {"__struct__": "SItemPortLoadoutManualParams", "entries": [{"itemPortName": "p", "entityClassName": "from_record"}]}}))
        }
        fn record_name(&self, r: &Value) -> Option<String> {
            Some(format!("Loot.{}", r[2].as_str()?))
        }
    }

    fn rf(g: &str) -> Value {
        json!(["ref", 1, g])
    }

    #[test]
    fn class_loot_config_needs_a_table() {
        let none = json!({"Components": [{"__struct__": "LootGenerationComponentParams", "lootConfig": {"lootTable": ["ref", 4294967295u64, "00000000-0000-0000-0000-000000000000"], "lootTableV3": null}}]});
        assert!(read(&none, &R).loot.is_none());
        let v3 = json!({"Components": [{"__struct__": "LootGenerationComponentParams", "lootConfig": {"lootTableV3": rf("t3"), "lootConstraints": {"x": 1}}}]});
        let l = read(&v3, &R).loot.unwrap();
        assert_eq!((l.table, l.table_v3.as_deref(), l.constraints["x"].as_i64()), (None, Some("Loot.t3"), Some(1)));
    }

    #[test]
    fn manual_loadout_entries_nest_and_carry_inventory() {
        let rec = json!({"Components": [
            {"__struct__": "SCItemInventoryContainerComponentParams"},
            {"__struct__": "SEntityComponentDefaultLoadoutParams", "loadout": {"__struct__": "SItemPortLoadoutManualParams",
                "InventoryItems": [{"entityClassName": "", "entityClassReference": rf("a"), "amount": 3}],
                "entries": [
                    {"itemPortName": "slot1", "entityClassName": "gun", "inventoryContainer": {"inventoryItems": [{"entityClassName": "mag", "amount": 2}]},
                     "loadout": {"__struct__": "SItemPortLoadoutManualParams", "entries": [{"itemPortName": "optic", "entityClassReference": rf("scope")}]}},
                    {"itemPortName": "empty", "entityClassName": ""}]}}]});
        let c = read(&rec, &R);
        assert!(c.inventory && !c.ports);
        let got: Vec<(&str, i64, &str, &str)> = c.fixed.iter().map(|i| (i.cls.as_str(), i.amount, i.port.as_str(), i.via)).collect();
        assert_eq!(got, vec![("cls_a", 3, "", "loadout inventory"), ("gun", 1, "slot1", "loadout"), ("mag", 2, "slot1", "loadout inventory"), ("cls_scope", 1, "optic", "loadout")]);
        assert!(c.unresolved.is_empty());
    }

    #[test]
    fn record_xml_random_and_outfit_loadouts() {
        let mk = |l: Value| json!({"Components": [{"__struct__": "SEntityComponentDefaultLoadoutParams", "loadout": l}]});
        let c = read(&mk(json!({"__struct__": "SItemPortLoadoutRecordParams", "loadoutRecord": rf("rec1")})), &R);
        assert_eq!(c.fixed.iter().map(|i| i.cls.as_str()).collect::<Vec<_>>(), vec!["from_record"]);
        let c = read(&mk(json!({"__struct__": "SItemPortLoadoutRecordParams", "loadoutRecord": rf("missing")})), &R);
        assert!(c.fixed.is_empty() && c.unresolved == vec!["loadout record"]);
        assert_eq!(read(&mk(json!({"__struct__": "SItemPortLoadoutXMLParams", "loadoutPath": "a/b.xml"})), &R).unresolved, vec!["xml:a/b.xml"]);
        assert_eq!(read(&mk(json!({"__struct__": "SItemPortLoadoutRandomGeneratedParams"})), &R).unresolved, vec!["random generated"]);
        let o = read(&mk(json!({"__struct__": "SItemPortLoadoutOutfitParams", "BaseLoadout": {"__struct__": "SItemPortLoadoutManualParams", "entries": [{"itemPortName": "p", "entityClassName": "base"}]}})), &R);
        assert_eq!((o.fixed.len(), o.unresolved.as_slice()), (1, &["outfit".to_string()][..]));
    }

    #[test]
    fn port_defaults_recurse_through_child_items() {
        let rec = json!({"Components": [{"__struct__": "SItemPortContainerComponentParams", "Ports": [
            {"Name": "a", "defaultItem": {"itemPort": "a", "entityClass": rf("x"), "childItems": [{"itemPort": "b", "entityClass": rf("y")}]}},
            {"Name": "b", "defaultItem": {"itemPort": "", "entityClass": ["ref", 4294967295u64, "00000000-0000-0000-0000-000000000000"]}}]}]});
        let c = read(&rec, &R);
        assert!(c.ports);
        assert_eq!(c.fixed.iter().map(|i| (i.cls.as_str(), i.port.as_str(), i.via)).collect::<Vec<_>>(), vec![("cls_x", "a", "port default"), ("cls_y", "b", "port default")]);
        assert_eq!(read(&json!({"Components": []}), &R), ClassFill::default());
    }
}
