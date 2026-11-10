//! The abstraction the swap / gear / outfit modules read item records through: the small part of `ItemStats` (record, reference,
//! localise, identity) and of `gearstats` (`facts`) they call.  There is ONE implementation, `ItemStats`; the trait exists so those
//! modules can be written against `&dyn ItemData` and tested with a stub.
//!
//! A record is a `serde_json::Value` made by `dcb::Value::to_json` (the `__struct__` key names the struct, a record reference is
//! `["ref", n, guid]`).

use serde_json::{json, Value};

use crate::itemstats::{self, ItemStats};

pub use crate::itemstats::{category, pretty, RECORD};

/// `{cls, name, type, subtype, size, grade, manufacturer, category}` of a class.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub cls: String,
    pub name: String,
    pub atype: String,
    pub subtype: String,
    pub size: Option<i64>,
    pub grade: Option<i64>,
    pub manufacturer: String,
    pub category: &'static str,
}

impl Identity {
    pub fn to_json(&self) -> Value {
        json!({"cls": self.cls, "name": self.name, "type": self.atype, "subtype": self.subtype, "size": self.size, "grade": self.grade,
            "manufacturer": self.manufacturer, "category": self.category})
    }
}

/// What the ports need from the item records (ItemStats).
pub trait ItemData: Sync {
    /// Data of `EntityClassDefinition.<cls>` read `depth` levels deep (ItemStats.record), None when the class is unknown.
    fn record(&self, cls: &str, depth: usize) -> Option<Value>;
    /// Record data behind a `["ref", n, guid]` reference (ItemStats._ref), None for the null reference.
    fn reference(&self, r: &Value, depth: usize) -> Option<Value>;
    /// English text of a `@key` localisation reference.
    fn localise(&self, key: &str) -> Option<String>;
    fn identity(&self, cls: &str) -> Identity;
    /// gearstats.facts of an item record.
    fn facts(&self, data: &Value) -> Value;
}

/// ItemStats._comp as an Option: the component dict with this `__struct__`.
pub fn comp<'a>(data: &'a Value, name: &str) -> Option<&'a Value> {
    let c = itemstats::comp(data, name);
    if c.is_null() {
        None
    } else {
        Some(c)
    }
}

/// `["ref", n, guid]` -> the guid, None for the null reference (n == 0xFFFFFFFF) or anything else.
pub fn ref_guid(r: &Value) -> Option<&str> {
    let a = r.as_array()?;
    if a.len() != 3 || a[1].as_u64() == Some(0xFFFF_FFFF) {
        return None;
    }
    a[2].as_str()
}

impl<'a> ItemData for ItemStats<'a> {
    fn record(&self, cls: &str, depth: usize) -> Option<Value> {
        ItemStats::record(self, cls, depth)
    }

    fn reference(&self, r: &Value, depth: usize) -> Option<Value> {
        self.rref(r, depth)
    }

    fn localise(&self, key: &str) -> Option<String> {
        self.localise_str(key)
    }

    fn identity(&self, cls: &str) -> Identity {
        let v = ItemStats::identity(self, cls);
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Identity {
            cls: s("cls"),
            name: s("name"),
            atype: s("type"),
            subtype: s("subtype"),
            size: v.get("size").and_then(Value::as_i64),
            grade: v.get("grade").and_then(Value::as_i64),
            manufacturer: s("manufacturer"),
            category: category(&s("type")),
        }
    }

    fn facts(&self, data: &Value) -> Value {
        Value::Object(crate::gearstats::facts(self, data))
    }
}
