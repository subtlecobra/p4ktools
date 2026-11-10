//! Python-flavoured helpers over `serde_json::Value` for the DataCore-driven catalogue code.
//!
//! The catalogue modules (itemstats, weaponstats, crafting, contracts ...) were originally written in Python and walk decoded records
//! as nested dicts with `.get(k) or {}` chains, truthiness tests and `%g` formats.  A record decoded by `Dcb::record_data` is turned into the same shape with
//! `Value::to_json` (dicts carry `__struct__`, references are `["ref", n, guid]`), and these helpers give the same semantics:
//! a missing key or a non-object yields `Null`, `truthy` is Python's `bool()`, `pyround` is `round(x, n)` (round half to even on the
//! exact binary value, which Rust's float formatting also does) and `fmt_g` is `"%.Ng" % x`.

use serde_json::{json, Map, Value};

pub type J = Value;

static NULL: Value = Value::Null;

/// `v[k]`, or Null when `v` is not an object or lacks the key.
pub fn g<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&NULL)
}

/// Python truthiness.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map_or(true, |x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A number that is not a bool.
pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Number or 0.0 (Python code that only reaches arithmetic with numbers).
pub fn f(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::Bool(b) => *b as u8 as f64,
        _ => 0.0,
    }
}

pub fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

pub fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// A list's non-null, truthy entries (Python `[x for x in lst if x]`).
pub fn items(v: &Value) -> impl Iterator<Item = &Value> {
    arr(v).iter().filter(|x| truthy(x))
}

pub fn is_obj(v: &Value) -> bool {
    v.is_object()
}

pub fn struct_of(v: &Value) -> &str {
    s(g(v, "__struct__"))
}

pub fn jf(x: f64) -> Value {
    if x.is_finite() {
        json!(x)
    } else {
        Value::Null
    }
}

pub fn jopt(x: Option<f64>) -> Value {
    x.map_or(Value::Null, jf)
}

/// Python `round(x, n)` for a float.
pub fn pyround(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

/// A JSON number that prints like Python's `int` when `x` is integral and was an int there.
pub fn ji(x: i64) -> Value {
    json!(x)
}

/// `"%.{prec}g" % x`.
pub fn fmt_g(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let prec = prec.max(1);
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let e = format!("{:.*e}", prec - 1, x);
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if exp < -4 || exp >= prec as i32 {
        let mant = trim_zeros(mant);
        format!("{}e{}{:02}", mant, if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let dec = (prec as i32 - 1 - exp).max(0) as usize;
        trim_zeros(&format!("{:.*}", dec, x)).to_string()
    }
}

fn trim_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// `"%g"` (precision 6).
pub fn gfmt(x: f64) -> String {
    fmt_g(x, 6)
}

/// `"%+.{prec}g"`.
pub fn fmt_gs(x: f64, prec: usize) -> String {
    let t = fmt_g(x, prec);
    if t.starts_with('-') {
        t
    } else {
        format!("+{}", t)
    }
}

/// Python `str()` of a decoded value as it appears in a label (`str(1.0)` = "1.0", ints plain, strings raw).
pub fn pystr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                pyfloat(n.as_f64().unwrap())
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python `repr(float)`.
pub fn pyfloat(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    if x == x.trunc() && x.abs() < 1e16 {
        return format!("{:.1}", x);
    }
    let a = x.abs();
    if a >= 1e16 || a < 1e-4 {
        let e = format!("{:e}", x);
        let (m, ex) = e.split_once('e').unwrap();
        let ex: i32 = ex.parse().unwrap();
        let m = if m.contains('.') { m.to_string() } else { m.to_string() };
        return format!("{}e{}{:02}", m, if ex < 0 { '-' } else { '+' }, ex.abs());
    }
    format!("{}", x)
}

/// Structural JSON equality with numbers compared by value (Python `1 == 1.0`).
pub fn jeq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| jeq(p, q)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map_or(false, |w| jeq(v, w))),
        _ => a == b,
    }
}

pub fn obj() -> Map<String, Value> {
    Map::new()
}

/// An `ItemStats` row: `{label, value, unit?, flag?, note?}`.
pub fn row(label: &str, value: Value, unit: &str, flag: &str, note: &str) -> Value {
    let mut r = Map::new();
    r.insert("label".into(), json!(label));
    r.insert("value".into(), value);
    if !unit.is_empty() {
        r.insert("unit".into(), json!(unit));
    }
    if !flag.is_empty() {
        r.insert("flag".into(), json!(flag));
    }
    if !note.is_empty() {
        r.insert("note".into(), json!(note));
    }
    Value::Object(r)
}

pub fn row2(label: &str, value: Value, unit: &str) -> Value {
    row(label, value, unit, "", "")
}

pub fn row1(label: &str, value: Value) -> Value {
    row(label, value, "", "", "")
}

/// A section: rows that are not None and whose value is not None; None when nothing is left.
pub fn section(title: &str, rows: Vec<Option<Value>>) -> Option<Value> {
    let rows: Vec<Value> = rows.into_iter().flatten().filter(|r| !g(r, "value").is_null()).collect();
    if rows.is_empty() {
        None
    } else {
        Some(json!({"title": title, "rows": rows}))
    }
}
