//! Reader for the DataCore binary (Game2.dcb), format version 8.  Lazy where it matters: the definition tables
//! are decoded when the file is opened (a few milliseconds), record and instance data only when asked for.
//!
//! The layout is CONFIRMED against the engine's DataCore loader and checked on the live file: header of 30 u32, then the struct / property / enum / mapping / record tables, the value
//! arrays (in the file order of `VALUE_ORDER`), the TEXT string table (value strings), the BLOB string table (definition names), then
//! the struct instance data.  Version 8 only; anything else is an error.  Callers pass the file's bytes (formats never touch the archive).
//! Instance layouts follow `struct_size` (inline classes expand in place, arrays and pointers take 8 bytes): EMPIRICAL.
//!
//! Strings are decoded with `String::from_utf8_lossy`, decoded instances are `Value` trees (an object is an `Obj` with its fields in
//! property order; strong / weak / reference values are tuples), and a missing key, a bad index or a truncated table is an `Err`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde_json::json;

use crate::error::*;

// property data types (low 16 bits of SDataCorePropertyDefinition.dataType)
pub const T_BOOL: u16 = 1;
pub const T_I8: u16 = 2;
pub const T_I16: u16 = 3;
pub const T_I32: u16 = 4;
pub const T_I64: u16 = 5;
pub const T_U8: u16 = 6;
pub const T_U16: u16 = 7;
pub const T_U32: u16 = 8;
pub const T_U64: u16 = 9;
pub const T_STRING: u16 = 0x0A;
pub const T_F32: u16 = 0x0B;
pub const T_F64: u16 = 0x0C;
pub const T_LOCALE: u16 = 0x0D;
pub const T_GUID: u16 = 0x0E;
pub const T_ENUM: u16 = 0x0F;
pub const T_CLASS: u16 = 0x0010;
pub const T_STRONG: u16 = 0x0110;
pub const T_WEAK: u16 = 0x0210;
pub const T_REF: u16 = 0x0310;

/// conversion type (u16 at property +8): 0 = single value, 1 = array of class-ish, 2 = array of simple
pub const CONV_SINGLE: u16 = 0;

pub const NULL_PTR: u32 = 0xFFFF_FFFF;
const HDR: usize = 120;
const STRUCT_SZ: usize = 16;
const PROP_SZ: usize = 12;
const ENUM_SZ: usize = 8;
const MAP_SZ: usize = 8;
pub const REC_SZ: usize = 36;

/// Value-array sections in FILE order: (name, index into the header counts, element size).  The header counts start with `bool`; the
/// file order below was measured (EMPIRICAL).
const VALUE_ORDER: [(&str, usize, usize); 19] = [
    ("i8", 1, 1), ("i16", 2, 2), ("i32", 3, 4), ("i64", 4, 8), ("u8", 5, 1), ("u16", 6, 2), ("u32", 7, 4), ("u64", 8, 8),
    ("bool", 0, 1), ("f32", 9, 4), ("f64", 10, 8), ("guid", 11, 16), ("string", 12, 4), ("locale", 13, 4), ("enum", 14, 4),
    ("strong", 15, 8), ("weak", 16, 8), ("ref", 17, 20), ("enumopt", 18, 4),
];
const ENUMOPT: usize = 18;

/// Section index of an array property of data type `t` (None for class arrays and unknown types).
fn array_section(t: u16) -> Option<usize> {
    Some(match t {
        T_I8 => 0,
        T_I16 => 1,
        T_I32 => 2,
        T_I64 => 3,
        T_U8 => 4,
        T_U16 => 5,
        T_U32 => 6,
        T_U64 => 7,
        T_BOOL => 8,
        T_F32 => 9,
        T_F64 => 10,
        T_GUID => 11,
        T_STRING => 12,
        T_LOCALE => 13,
        T_ENUM => 14,
        T_STRONG => 15,
        T_WEAK => 16,
        T_REF => 17,
        _ => return None,
    })
}

/// Size in bytes of an inline value of type `t` (classes are sized by their struct).
pub fn inline_size(t: u16) -> Option<usize> {
    Some(match t {
        T_BOOL | T_I8 | T_U8 => 1,
        T_I16 | T_U16 => 2,
        T_I32 | T_U32 | T_STRING | T_F32 | T_LOCALE | T_ENUM => 4,
        T_I64 | T_U64 | T_F64 | T_STRONG | T_WEAK => 8,
        T_GUID => 16,
        T_REF => 20,
        _ => return None,
    })
}

pub fn type_name(t: u16) -> Option<&'static str> {
    Some(match t {
        T_BOOL => "Boolean",
        T_I8 => "Int8",
        T_I16 => "Int16",
        T_I32 => "Int32",
        T_I64 => "Int64",
        T_U8 => "UInt8",
        T_U16 => "UInt16",
        T_U32 => "UInt32",
        T_U64 => "UInt64",
        T_STRING => "String",
        T_F32 => "Single",
        T_F64 => "Double",
        T_LOCALE => "Locale",
        T_GUID => "Guid",
        T_ENUM => "Enum",
        T_CLASS => "Class",
        T_STRONG => "StrongPointer",
        T_WEAK => "WeakPointer",
        T_REF => "Reference",
        _ => return None,
    })
}

/// Record/GUID bytes -> canonical string.  The file stores the GUID with the first three groups byte-reversed relative to the textual
/// form, in the order the engine formats it.
pub fn guid_str(b: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[7], b[6], b[5], b[4], b[3], b[2], b[1], b[0], b[15], b[14], b[13], b[12], b[11], b[10], b[9], b[8]
    )
}

/// A decoded value.  `Strong` / `Weak` carry (struct index, instance index), `Ref` (index, record GUID text).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Guid(String),
    Strong(u32, u32),
    Weak(u32, u32),
    Ref(u32, String),
    /// a class instance not expanded because `maxdepth` was reached: (struct index, instance index)
    ClassRef(u32, u32),
    List(Vec<Value>),
    Obj(Obj),
}

/// A decoded struct instance; `fields` are in property order (base struct first), a repeated name keeps its first position.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Obj {
    pub struct_name: String,
    pub fields: Vec<(String, Value)>,
}

impl Obj {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    fn set(&mut self, key: &str, v: Value) {
        match self.fields.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = v,
            None => self.fields.push((key.to_string(), v)),
        }
    }
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(o) => o.get(key),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }
    /// Numeric value as f64 (ints included), None for anything else.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Bool(b) => Some(*b as u8 as f64),
            _ => None,
        }
    }
    /// JSON form: tuples as lists, `Obj` as a dict with `__struct__`, non-finite floats as strings.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => json!(null),
            Value::Bool(b) => json!(b),
            Value::Int(i) => json!(i),
            Value::UInt(u) => json!(u),
            Value::Float(f) if f.is_nan() => json!("NaN"),
            Value::Float(f) if f.is_infinite() => json!(if *f > 0.0 { "Infinity" } else { "-Infinity" }),
            Value::Float(f) => json!(f),
            Value::Str(s) | Value::Guid(s) => json!(s),
            Value::Strong(a, b) => json!(["strong", a, b]),
            Value::Weak(a, b) => json!(["weak", a, b]),
            Value::Ref(a, g) => json!(["ref", a, g]),
            Value::ClassRef(a, b) => json!(["class", a, b]),
            Value::List(l) => serde_json::Value::Array(l.iter().map(Value::to_json).collect()),
            Value::Obj(o) => {
                let mut m = serde_json::Map::new();
                m.insert("__struct__".into(), json!(o.struct_name));
                for (k, v) in &o.fields {
                    m.insert(k.clone(), v.to_json());
                }
                serde_json::Value::Object(m)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    /// -1 = none
    pub parent: i32,
    pub count: u16,
    pub first: u16,
    pub node_type: u32,
}

#[derive(Debug, Clone)]
pub struct PropDef {
    pub name: String,
    /// struct (or enum) index for classes and enums
    pub s: u16,
    pub t: u16,
    pub c: u16,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub name: String,
    pub s: u16,
    pub t: u16,
    pub c: u16,
    /// byte offset inside an instance
    pub off: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub index: usize,
    pub name: String,
    pub file: String,
    pub tag: String,
    pub struct_idx: u32,
    pub struct_name: String,
    pub guid: String,
    pub instance: u16,
    pub size_field: u16,
}

/// What a graph walk over instances of a struct must look at (see `Dcb::walk_plan`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WalkKind {
    Ref,
    Refs,
    Weak,
    Weaks,
    Strong,
    Strongs,
    Classes,
}

pub struct Dcb {
    m: Vec<u8>,
    pub version: u32,
    pub n_struct: usize,
    pub n_prop: usize,
    pub n_enum: usize,
    pub n_map: usize,
    pub n_rec: usize,
    counts: [u32; 19],
    struct_off: usize,
    prop_off: usize,
    enum_off: usize,
    map_off: usize,
    pub rec_off: usize,
    arr_off: [usize; 19],
    text_off: usize,
    text_len: usize,
    blob_off: usize,
    blob_len: usize,
    pub data_off: usize,
    sdefs: Vec<StructDef>,
    pdefs: Vec<PropDef>,
    /// all properties of each struct in data order: base struct's first, then own
    props: Vec<Vec<PropDef>>,
    layouts: Vec<Vec<Layout>>,
    sizes: Vec<Option<usize>>,
    /// struct index -> (first instance offset, instance count)
    bases: HashMap<u32, (usize, usize)>,
    pub data_end: usize,
    names: OnceLock<HashMap<String, usize>>,
    rec_index: OnceLock<HashMap<String, Vec<usize>>>,
    plans: Mutex<HashMap<usize, Vec<(WalkKind, usize, usize)>>>,
    /// cache slot for `itemwear::floor_paths` (computed once per reader)
    pub floor_paths: OnceLock<HashSet<String>>,
}

impl Dcb {
    /// Parse the header and the definition tables of a version 8 DataCore file.
    pub fn open(buf: Vec<u8>) -> Result<Dcb> {
        let h: Vec<u32> = (0..30).map(|i| u32_at(&buf, 4 * i)).collect::<Result<_>>()?;
        if h[1] != 8 {
            return Err(Error::msg(format!("ValueError: only version 8 is implemented, file has {}", h[1])));
        }
        let (n_struct, n_prop, n_enum, n_map, n_rec) = (h[4] as usize, h[5] as usize, h[6] as usize, h[7] as usize, h[8] as usize);
        let mut counts = [0u32; 19];
        counts.copy_from_slice(&h[9..28]);
        let (text_len, blob_len) = (h[28] as usize, h[29] as usize);
        let mut o = HDR;
        let struct_off = o;
        o += n_struct * STRUCT_SZ;
        let prop_off = o;
        o += n_prop * PROP_SZ;
        let enum_off = o;
        o += n_enum * ENUM_SZ;
        let map_off = o;
        o += n_map * MAP_SZ;
        let rec_off = o;
        o += n_rec * REC_SZ;
        let mut arr_off = [0usize; 19];
        for (i, (_, hi, sz)) in VALUE_ORDER.iter().enumerate() {
            arr_off[i] = o;
            o += counts[*hi] as usize * sz;
        }
        let text_off = o;
        o += text_len;
        let blob_off = o;
        o += blob_len;
        let data_off = o;
        if data_off > buf.len() {
            return Err(Error::msg("ValueError: DataCore file is truncated"));
        }
        let mut d = Dcb {
            m: buf,
            version: h[1],
            n_struct,
            n_prop,
            n_enum,
            n_map,
            n_rec,
            counts,
            struct_off,
            prop_off,
            enum_off,
            map_off,
            rec_off,
            arr_off,
            text_off,
            text_len,
            blob_off,
            blob_len,
            data_off,
            sdefs: vec![],
            pdefs: vec![],
            props: vec![],
            layouts: vec![],
            sizes: vec![],
            bases: HashMap::new(),
            data_end: 0,
            names: OnceLock::new(),
            rec_index: OnceLock::new(),
            plans: Mutex::new(HashMap::new()),
            floor_paths: OnceLock::new(),
        };
        d.load_defs()?;
        Ok(d)
    }

    fn load_defs(&mut self) -> Result<()> {
        let mut sdefs = Vec::with_capacity(self.n_struct);
        for i in 0..self.n_struct {
            let o = self.struct_off + i * STRUCT_SZ;
            let n = u32_at(&self.m, o)?;
            sdefs.push(StructDef {
                name: self.blob(n)?.into_owned(),
                parent: u32_at(&self.m, o + 4)? as i32,
                count: u16_at(&self.m, o + 8)?,
                first: u16_at(&self.m, o + 10)?,
                node_type: u32_at(&self.m, o + 12)?,
            });
        }
        let mut pdefs = Vec::with_capacity(self.n_prop);
        for i in 0..self.n_prop {
            let o = self.prop_off + i * PROP_SZ;
            let n = u32_at(&self.m, o)?;
            pdefs.push(PropDef { name: self.blob(n)?.into_owned(), s: u16_at(&self.m, o + 4)?, t: u16_at(&self.m, o + 6)?, c: u16_at(&self.m, o + 8)? });
        }
        self.sdefs = sdefs;
        self.pdefs = pdefs;
        // properties in data order (base first); memoised, parents resolved on demand
        let mut props: Vec<Option<Vec<PropDef>>> = vec![None; self.n_struct];
        for si in 0..self.n_struct {
            self.resolve_props(si, &mut props, 0);
        }
        self.props = props.into_iter().map(|p| p.unwrap_or_default()).collect();
        // sizes (memoised recursion over inline classes)
        let mut sizes: Vec<Option<Option<usize>>> = vec![None; self.n_struct];
        for si in 0..self.n_struct {
            self.resolve_size(si, &mut sizes, 0);
        }
        self.sizes = sizes.into_iter().map(|s| s.flatten()).collect();
        let mut layouts = Vec::with_capacity(self.n_struct);
        for si in 0..self.n_struct {
            let mut off = 0usize;
            let mut lay = Vec::with_capacity(self.props[si].len());
            for p in &self.props[si] {
                lay.push(Layout { name: p.name.clone(), s: p.s, t: p.t, c: p.c, off });
                off += if p.c != CONV_SINGLE {
                    8
                } else if p.t == T_CLASS {
                    self.sizes.get(p.s as usize).copied().flatten().unwrap_or(0)
                } else {
                    inline_size(p.t).unwrap_or(0)
                };
            }
            layouts.push(lay);
        }
        self.layouts = layouts;
        // instance bases, in mapping order
        let mut b = self.data_off;
        for k in 0..self.n_map {
            let cnt = u32_at(&self.m, self.map_off + k * MAP_SZ)? as usize;
            let si = u32_at(&self.m, self.map_off + k * MAP_SZ + 4)?;
            self.bases.insert(si, (b, cnt));
            b += cnt * self.struct_size(si as usize).unwrap_or(0);
        }
        self.data_end = b;
        Ok(())
    }

    fn resolve_props(&self, si: usize, memo: &mut Vec<Option<Vec<PropDef>>>, depth: usize) {
        if memo[si].is_some() || depth > 64 {
            return;
        }
        let d = &self.sdefs[si];
        let mut p = vec![];
        if d.parent >= 0 && (d.parent as usize) < self.n_struct {
            self.resolve_props(d.parent as usize, memo, depth + 1);
            p = memo[d.parent as usize].clone().unwrap_or_default();
        }
        for k in 0..d.count as usize {
            if let Some(pd) = self.pdefs.get(d.first as usize + k) {
                p.push(pd.clone());
            }
        }
        memo[si] = Some(p);
    }

    fn resolve_size(&self, si: usize, memo: &mut Vec<Option<Option<usize>>>, depth: usize) -> Option<usize> {
        if let Some(v) = memo[si] {
            return v;
        }
        if depth > 64 {
            return None;
        }
        let mut z = Some(0usize);
        for p in &self.props[si] {
            let add = if p.c != CONV_SINGLE {
                Some(8)
            } else if p.t == T_CLASS {
                if (p.s as usize) < self.n_struct {
                    self.resolve_size(p.s as usize, memo, depth + 1)
                } else {
                    None
                }
            } else {
                inline_size(p.t)
            };
            z = match (z, add) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            };
        }
        memo[si] = Some(z);
        z
    }

    /// The whole file.
    pub fn bytes(&self) -> &[u8] {
        &self.m
    }

    // ---- strings ------------------------------------------------------------------------------
    fn cstr(&self, base: usize, len: usize, off: u32) -> Result<Cow<'_, str>> {
        let start = base + off as usize;
        let limit = (base + len).min(self.m.len());
        let hay = self.m.get(start..limit).ok_or_else(|| Error::msg("string offset out of range"))?;
        let end = hay.iter().position(|&b| b == 0).ok_or_else(|| Error::msg("unterminated string"))?;
        Ok(String::from_utf8_lossy(&hay[..end]))
    }

    /// Value string (TEXT table).
    pub fn text(&self, off: u32) -> Result<Cow<'_, str>> {
        self.cstr(self.text_off, self.text_len, off)
    }

    /// Definition name (BLOB table).
    pub fn blob(&self, off: u32) -> Result<Cow<'_, str>> {
        self.cstr(self.blob_off, self.blob_len, off)
    }

    // ---- definition tables --------------------------------------------------------------------
    pub fn struct_def(&self, i: usize) -> Result<&StructDef> {
        self.sdefs.get(i).ok_or_else(|| Error::msg(format!("IndexError: struct {}", i)))
    }

    pub fn struct_name(&self, i: usize) -> Result<&str> {
        Ok(&self.struct_def(i)?.name)
    }

    /// `{struct name: index}` (a repeated name keeps the last index).
    pub fn struct_names(&self) -> &HashMap<String, usize> {
        self.names.get_or_init(|| self.sdefs.iter().enumerate().map(|(i, s)| (s.name.clone(), i)).collect())
    }

    /// All properties of struct `si` in data order: base struct's first, then own.
    pub fn props(&self, si: usize) -> Result<&[PropDef]> {
        self.props.get(si).map(|v| v.as_slice()).ok_or_else(|| Error::msg(format!("IndexError: struct {}", si)))
    }

    pub fn struct_size(&self, si: usize) -> Result<usize> {
        self.sizes.get(si).copied().flatten().ok_or_else(|| Error::msg(format!("KeyError: size of struct {}", si)))
    }

    /// (enum name, option names)
    pub fn enum_def(&self, i: usize) -> Result<(String, Vec<String>)> {
        if i >= self.n_enum {
            return Err(Error::msg(format!("IndexError: enum {}", i)));
        }
        let o = self.enum_off + i * ENUM_SZ;
        let n = u32_at(&self.m, o)?;
        let cnt = u16_at(&self.m, o + 4)? as usize;
        let first = u16_at(&self.m, o + 6)? as usize;
        let mut opts = vec![];
        for k in 0..cnt {
            let so = u32_at(&self.m, self.arr_off[ENUMOPT] + 4 * (first + k))?;
            opts.push(self.blob(so)?.into_owned());
        }
        Ok((self.blob(n)?.into_owned(), opts))
    }

    // ---- instances ----------------------------------------------------------------------------
    /// (offset of the first instance, instance count) of struct `si`, if the file has instances of it.
    pub fn instance_base(&self, si: usize) -> Option<(usize, usize)> {
        self.bases.get(&(si as u32)).copied()
    }

    pub fn instance_offset(&self, si: usize, idx: usize) -> Result<usize> {
        let (b, cnt) = self.instance_base(si).ok_or_else(|| Error::msg(format!("KeyError: {}", si)))?;
        if idx >= cnt {
            return Err(Error::msg(format!("IndexError: instance {} of struct {} (count {})", idx, si, cnt)));
        }
        Ok(b + idx * self.struct_size(si)?)
    }

    // ---- records ------------------------------------------------------------------------------
    fn rec_at(&self, i: usize) -> Result<usize> {
        if i >= self.n_rec {
            return Err(Error::msg(format!("IndexError: record {}", i)));
        }
        Ok(self.rec_off + i * REC_SZ)
    }

    pub fn record(&self, i: usize) -> Result<Record> {
        let o = self.rec_at(i)?;
        let si = u32_at(&self.m, o + 12)?;
        Ok(Record {
            index: i,
            name: self.blob(u32_at(&self.m, o)?)?.into_owned(),
            file: self.text(u32_at(&self.m, o + 4)?)?.into_owned(),
            tag: self.blob(u32_at(&self.m, o + 8)?)?.into_owned(),
            struct_idx: si,
            struct_name: self.struct_name(si as usize)?.to_string(),
            guid: guid_str(&self.m[o + 16..o + 32]),
            instance: u16_at(&self.m, o + 32)?,
            size_field: u16_at(&self.m, o + 34)?,
        })
    }

    /// Record name (blob string) of record `i`.
    pub fn record_name(&self, i: usize) -> Result<Cow<'_, str>> {
        let o = self.rec_at(i)?;
        self.blob(u32_at(&self.m, o)?)
    }

    /// Record file path (text string) of record `i`.
    pub fn record_file(&self, i: usize) -> Result<Cow<'_, str>> {
        let o = self.rec_at(i)?;
        self.text(u32_at(&self.m, o + 4)?)
    }

    /// `{record name: [record index]}` for every record (one pass over the record table), cached.
    pub fn record_index(&self) -> &HashMap<String, Vec<usize>> {
        self.rec_index.get_or_init(|| {
            let mut idx: HashMap<String, Vec<usize>> = HashMap::new();
            for i in 0..self.n_rec {
                if let Ok(n) = self.record_name(i) {
                    match idx.get_mut(n.as_ref()) {
                        Some(v) => v.push(i),
                        None => {
                            idx.insert(n.into_owned(), vec![i]);
                        }
                    }
                }
            }
            idx
        })
    }

    /// Record indexes whose name equals / contains `name`.
    pub fn find_records(&self, name: &str, exact: bool) -> Vec<usize> {
        if exact {
            return self.record_index().get(name).cloned().unwrap_or_default();
        }
        (0..self.n_rec).filter(|&i| self.record_name(i).map_or(false, |n| n.contains(name))).collect()
    }

    /// The 16 stored GUID bytes of record `i` (compare with the bytes of a reference; `guid_str` makes the text).
    pub fn record_guid_bytes(&self, i: usize) -> Result<&[u8]> {
        let o = self.rec_at(i)?;
        Ok(&self.m[o + 16..o + 32])
    }

    /// (struct index, instance index) of record `i`'s main instance.
    pub fn record_root(&self, i: usize) -> Result<(usize, usize)> {
        let o = self.rec_at(i)?;
        Ok((u32_at(&self.m, o + 12)? as usize, u16_at(&self.m, o + 32)? as usize))
    }

    // ---- value decoding -----------------------------------------------------------------------
    fn arr_value(&self, t: u16, i: usize) -> Result<Value> {
        let sec = array_section(t).ok_or_else(|| Error::msg(format!("KeyError: array of type {:#x}", t)))?;
        let sz = VALUE_ORDER[sec].2;
        let o = self.arr_off[sec] + i * sz;
        self.value_at(t, o)
    }

    /// Decode one inline non-class value of data type `t` at absolute offset `off`.  Strong / weak / reference values stay tuples
    /// (`Strong(struct, instance)`, `Ref(n, guid)`), enums and strings are text.
    pub fn value_at(&self, t: u16, off: usize) -> Result<Value> {
        let m = &self.m;
        Ok(match t {
            T_BOOL => Value::Bool(u8_at(m, off)? != 0),
            T_I8 => Value::Int(u8_at(m, off)? as i8 as i64),
            T_I16 => Value::Int(i16_at(m, off)? as i64),
            T_I32 => Value::Int(u32_at(m, off)? as i32 as i64),
            T_I64 => Value::Int(i64_at(m, off)?),
            T_U8 => Value::UInt(u8_at(m, off)? as u64),
            T_U16 => Value::UInt(u16_at(m, off)? as u64),
            T_U32 => Value::UInt(u32_at(m, off)? as u64),
            T_U64 => Value::UInt(u64_at(m, off)?),
            T_F32 => Value::Float(f32_at(m, off)?),
            T_F64 => Value::Float(f64_at(m, off)?),
            T_STRING | T_LOCALE | T_ENUM => Value::Str(self.text(u32_at(m, off)?)?.into_owned()),
            T_GUID => Value::Guid(guid_str(m.get(off..off + 16).ok_or_else(|| Error::msg("guid out of range"))?)),
            T_STRONG => Value::Strong(u32_at(m, off)?, u32_at(m, off + 4)?),
            T_WEAK => Value::Weak(u32_at(m, off)?, u32_at(m, off + 4)?),
            T_REF => Value::Ref(u32_at(m, off)?, guid_str(m.get(off + 4..off + 20).ok_or_else(|| Error::msg("guid out of range"))?)),
            _ => return Err(Error::msg(format!("KeyError: inline type {:#x}", t))),
        })
    }

    /// Alias kept for the DataCore browser.
    pub fn read_inline(&self, t: u16, off: usize) -> Result<Value> {
        self.value_at(t, off)
    }

    /// (count, first index) of an array property stored at `off`.
    pub fn array_header(&self, off: usize) -> Result<(usize, usize)> {
        Ok((u32_at(&self.m, off)? as usize, u32_at(&self.m, off + 4)? as usize))
    }

    /// Values of the simple (non-class) array elements `first .. first+count` of data type `t`.
    pub fn array_items(&self, t: u16, first: usize, count: usize) -> Result<Vec<Value>> {
        (0..count).map(|k| self.arr_value(t, first + k)).collect()
    }

    /// Decode struct instance `idx` of struct `si`.  Strong pointers are followed while `depth + 1 <= maxdepth` (null pointers become
    /// `Null`), beyond that they stay `Strong` tuples (use `deref`); class arrays beyond the limit become `ClassRef`.
    pub fn read_instance(&self, si: usize, idx: usize, depth: usize, maxdepth: usize) -> Result<Value> {
        let off = self.instance_offset(si, idx)?;
        self.read_at(si, off, depth, maxdepth)
    }

    fn read_at(&self, si: usize, mut off: usize, depth: usize, maxdepth: usize) -> Result<Value> {
        let mut out = Obj { struct_name: self.struct_name(si)?.to_string(), fields: vec![] };
        for p in self.props(si)? {
            if p.c != CONV_SINGLE {
                let (cnt, first) = self.array_header(off)?;
                off += 8;
                let v = if p.t == T_CLASS {
                    let mut l = Vec::with_capacity(cnt);
                    for k in 0..cnt {
                        l.push(if depth < maxdepth {
                            self.read_at(p.s as usize, self.instance_offset(p.s as usize, first + k)?, depth + 1, maxdepth)?
                        } else {
                            Value::ClassRef(p.s as u32, (first + k) as u32)
                        });
                    }
                    l
                } else {
                    let mut l = self.array_items(p.t, first, cnt)?;
                    if depth < maxdepth {
                        for x in l.iter_mut() {
                            *x = self.deref(x.clone(), depth + 1, maxdepth)?;
                        }
                    }
                    l
                };
                out.set(&p.name, Value::List(v));
                continue;
            }
            if p.t == T_CLASS {
                let v = self.read_at(p.s as usize, off, depth + 1, maxdepth)?;
                off += self.struct_size(p.s as usize)?;
                out.set(&p.name, v);
                continue;
            }
            let sz = inline_size(p.t).ok_or_else(|| Error::msg(format!("KeyError: type {:#x}", p.t)))?;
            let mut v = self.value_at(p.t, off)?;
            off += sz;
            if matches!(v, Value::Strong(..) | Value::Weak(..)) && depth < maxdepth {
                v = self.deref(v, depth + 1, maxdepth)?;
            }
            out.set(&p.name, v);
        }
        Ok(Value::Obj(out))
    }

    /// Resolve a `Strong` / `Weak` value to its instance (`Null` for a null pointer); other values pass through.
    pub fn deref(&self, v: Value, depth: usize, maxdepth: usize) -> Result<Value> {
        match v {
            Value::Strong(s, i) | Value::Weak(s, i) => {
                if s == NULL_PTR {
                    Ok(Value::Null)
                } else {
                    self.read_instance(s as usize, i as usize, depth, maxdepth)
                }
            }
            v => Ok(v),
        }
    }

    pub fn record_data(&self, rec: &Record, maxdepth: usize) -> Result<Value> {
        self.read_instance(rec.struct_idx as usize, rec.instance as usize, 0, maxdepth)
    }

    // ---- bulk helpers (geometry -> material pairs) --------------------------------------------
    /// Byte offset of an inline (non-array) property inside an instance of struct `si`.
    pub fn field_offset(&self, si: usize, prop_name: &str) -> Result<usize> {
        self.prop_layout(si)?.iter().find(|l| l.name == prop_name).map(|l| l.off).ok_or_else(|| Error::msg(format!("KeyError: {}", prop_name)))
    }

    /// `[(name, struct/enum index, data type, conversion, byte offset)]` of struct `si` in data order (base struct first).
    pub fn prop_layout(&self, si: usize) -> Result<&[Layout]> {
        self.layouts.get(si).map(|v| v.as_slice()).ok_or_else(|| Error::msg(format!("IndexError: struct {}", si)))
    }

    /// `(instance index, absolute offset)` of every instance of the named struct.
    pub fn iter_instance_offsets(&self, struct_name: &str) -> Result<impl Iterator<Item = (usize, usize)>> {
        let si = *self.struct_names().get(struct_name).ok_or_else(|| Error::msg(format!("KeyError: {}", struct_name)))?;
        let (base, cnt) = self.instance_base(si).ok_or_else(|| Error::msg(format!("KeyError: {}", si)))?;
        let z = self.struct_size(si)?;
        Ok((0..cnt).map(move |k| (k, base + k * z)))
    }

    /// `(container, index, geometry_path, material_path)` of every SGeometryDataParams in the file: those embedded in SGeometryNodeParams
    /// instances (root-less sub nodes) and in the root SGeometryResourceParams.Geometry node.  Paths are the raw strings (may be empty).
    pub fn geometry_pairs(&self) -> Result<Vec<(&'static str, usize, String, String)>> {
        let n = self.struct_names();
        let need = |k: &str| n.get(k).copied().ok_or_else(|| Error::msg(format!("KeyError: {}", k)));
        let data_in_node = self.field_offset(need("SGeometryNodeParams")?, "Geometry")?;
        let data_in_root = self.field_offset(need("SGeometryResourceParams")?, "Geometry")? + data_in_node;
        let gp = need("SGeometryDataParams")?;
        let g_off = self.field_offset(gp, "Geometry")?; // inline GlobalResourceGeometry
        let m_off = self.field_offset(gp, "Material")?; // inline GlobalResourceMaterial
        let mut out = vec![];
        for (cont, base) in [("SGeometryNodeParams", data_in_node), ("SGeometryResourceParams", data_in_root)] {
            for (k, off) in self.iter_instance_offsets(cont)? {
                let g = u32_at(&self.m, off + base + g_off)?;
                let mt = u32_at(&self.m, off + base + m_off)?;
                out.push((cont, k, self.text(g)?.into_owned(), self.text(mt)?.into_owned()));
            }
        }
        Ok(out)
    }

    /// What a graph walk over instances of struct `si` must look at: `[(kind, offset, struct index)]`, inline classes flattened (their
    /// offsets added).  Only classes whose own plan is non-empty are kept, so a walk skips plain value structs.
    pub fn walk_plan(&self, si: usize) -> Result<Vec<(WalkKind, usize, usize)>> {
        if let Some(p) = self.plans.lock().unwrap().get(&si) {
            return Ok(p.clone());
        }
        self.plans.lock().unwrap().insert(si, vec![]); // a struct does not contain itself
        let mut out = vec![];
        for l in self.prop_layout(si)? {
            let (s, t, off) = (l.s as usize, l.t, l.off);
            if l.c != CONV_SINGLE {
                match t {
                    T_CLASS => {
                        if !self.walk_plan(s)?.is_empty() {
                            out.push((WalkKind::Classes, off, s));
                        }
                    }
                    T_REF => out.push((WalkKind::Refs, off, 0)),
                    T_WEAK => out.push((WalkKind::Weaks, off, 0)),
                    T_STRONG => out.push((WalkKind::Strongs, off, 0)),
                    _ => {}
                }
            } else {
                match t {
                    T_CLASS => {
                        for (k, o, s2) in self.walk_plan(s)? {
                            out.push((k, o + off, s2));
                        }
                    }
                    T_REF => out.push((WalkKind::Ref, off, 0)),
                    T_WEAK => out.push((WalkKind::Weak, off, 0)),
                    T_STRONG => out.push((WalkKind::Strong, off, 0)),
                    _ => {}
                }
            }
        }
        self.plans.lock().unwrap().insert(si, out.clone());
        Ok(out)
    }

    /// Absolute file offset of the value array of data type `t` (strong / weak pointers, references ...), for graph scans.
    pub fn array_base(&self, t: u16) -> Option<usize> {
        array_section(t).map(|s| self.arr_off[s])
    }

    /// The `count` property definitions a struct declares itself (`first .. first + count` of the property table).
    pub fn own_props(&self, si: usize) -> Result<&[PropDef]> {
        let sd = self.struct_def(si)?;
        let (a, n) = (sd.first as usize, sd.count as usize);
        self.pdefs.get(a..a + n).ok_or_else(|| Error::msg(format!("IndexError: properties of struct {}", si)))
    }

    pub fn counts(&self) -> &[u32; 19] {
        &self.counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_text_reverses_first_groups() {
        let b: Vec<u8> = (0..16).collect();
        assert_eq!(guid_str(&b), "07060504-0302-0100-0f0e-0d0c0b0a0908");
    }

    #[test]
    fn rejects_other_versions() {
        let mut b = vec![0u8; 200];
        b[4] = 7;
        assert!(Dcb::open(b).is_err());
    }

    #[test]
    fn obj_set_replaces_in_place() {
        let mut o = Obj::default();
        o.set("a", Value::Int(1));
        o.set("b", Value::Int(2));
        o.set("a", Value::Int(3));
        assert_eq!(o.fields.len(), 2);
        assert_eq!(o.get("a"), Some(&Value::Int(3)));
        assert_eq!(Value::Obj(o).to_json()["b"], json!(2));
    }
}
