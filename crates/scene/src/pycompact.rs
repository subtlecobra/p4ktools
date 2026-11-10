//! Python's `json.dumps(v, separators=(",", ":"))` spelling for the JSON header of binary blobs (floats as `repr`: `1e-06`, not serde's
//! `1e-6`), so the replies stay byte-identical to the ones of the original Python implementation.  Strings are written as serde does (the headers hold ASCII names only).

use serde_json::Value;

fn float_repr(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() { "NaN".into() } else if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let s = format!("{:?}", x);
    match s.split_once('e') {
        None => s,
        Some((m, e)) => match e.strip_prefix('-') {
            Some(d) => format!("{}e-{:0>2}", m, d),
            None => format!("{}e+{:0>2}", m, e),
        },
    }
}

fn write(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => out.push_str(&i.to_string()),
            (_, Some(u)) => out.push_str(&u.to_string()),
            _ => out.push_str(&float_repr(n.as_f64().unwrap_or(0.0))),
        },
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap()),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                write(x, out);
            }
            out.push('}');
        }
    }
}

pub fn compact(v: &Value) -> Vec<u8> {
    let mut o = String::new();
    write(v, &mut o);
    o.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_float_spelling() {
        assert_eq!(compact(&json!([1e-6, 1.0, 1, 1e16, -0.5])), b"[1e-06,1.0,1,1e+16,-0.5]");
    }
}
