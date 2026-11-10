//! Serialise a DataCore record to indented XML text, in the shape of the DataCore XML exports tools produce.
//!
//! Shape (a presentation choice, not a file format): the root element is named after the record and carries `__type` (struct), `__ref`
//! (GUID) and `__path` (virtual file path); every scalar property is an attribute; a class or strong-pointer member is a child element
//! named after the property (a pointer adds `__type` and `__idx`, its instance index, which is the N of the `@Type[N]` text that weak pointers print, because the instance may be a derived struct); an array is an element named after
//! the property whose children are the elements (class / pointer arrays: one element per instance, named after its struct; simple arrays:
//! `<String>x</String>`, `<Single>1.5</Single>` ...).  Strong pointers own their target, so they are expanded in place; weak pointers and
//! references are not followed, they are written as the target record's name when it can be found (`ref_name`, `weak_name` callbacks)
//! and as the GUID / `@Struct[index]` otherwise.  A null pointer is `null`.
//!
//! The output is capped (`limit` characters): when it is reached the open elements are closed and a comment says so, so the text stays
//! well-formed.  Data layout facts come from dcb.rs (CONFIRMED against the engine loader there); the XML shape here is ours.

use crate::dcb::*;
use crate::error::*;

pub const NULL_GUID: &str = "00000000-0000-0000-0000-000000000000";
const MAX_DEPTH: usize = 160;

/// An XML-safe tag / attribute name for `s` (invalid characters become '_', a leading digit gets a prefix).
pub fn xml_name(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '.' || c == '-' || c == ':' { c } else { '_' }).collect();
    let t = if t.is_empty() { "_".to_string() } else { t };
    let first = t.chars().next().unwrap();
    if first.is_numeric() || first == '-' || first == '.' {
        format!("_{}", t)
    } else {
        t
    }
}

pub fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\n', "&#10;").replace('\r', "&#13;").replace('\t', "&#9;")
}

pub fn esc_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// C `printf("%.{p}g")` of a finite value.
fn fmt_g(v: f64, p: usize) -> String {
    let e = format!("{:.*e}", p - 1, v); // d.ddde[-]x, correctly rounded
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    let body = if exp < -4 || exp >= p as i32 {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        format!("{}e{}{:02}", strip(&m), if exp < 0 { '-' } else { '+' }, exp.abs())
    } else if exp >= 0 {
        let int_len = exp as usize + 1;
        let s = if digits.len() > int_len { format!("{}.{}", &digits[..int_len], &digits[int_len..]) } else { digits.clone() };
        strip(&s)
    } else {
        strip(&format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits))
    };
    if neg {
        format!("-{}", body)
    } else {
        body
    }
}

/// Shortest round-trip float text, as Python's `repr(float)` prints it.
fn py_repr(v: f64) -> String {
    let e = format!("{:e}", v);
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let body = if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() > int_len {
                format!("{}.{}", &digits[..int_len], &digits[int_len..])
            } else {
                format!("{}{}.0", digits, "0".repeat(int_len - digits.len()))
            }
        } else {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
        }
    } else {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        format!("{}e{}{:02}", m, if exp < 0 { '-' } else { '+' }, exp.abs())
    };
    if neg {
        format!("-{}", body)
    } else {
        body
    }
}

/// Shortest text that reads back as the same float32 (or float64); inf / nan as words.
pub fn fmt_float(v: f64, single: bool) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if !single {
        return py_repr(v);
    }
    for p in 6..10 {
        let s = fmt_g(v, p);
        if let Ok(f) = s.parse::<f64>() {
            if (f as f32).to_bits() == (v as f32).to_bits() {
                return s;
            }
        }
    }
    fmt_g(v, 9)
}

/// Result of `record_xml`.
#[derive(Debug, Clone, PartialEq)]
pub struct XmlInfo {
    pub truncated: bool,
    pub elements: usize,
    pub chars: usize,
}

struct Writer<'a> {
    d: &'a Dcb,
    ref_name: &'a dyn Fn(&str) -> Option<String>,
    weak_name: &'a dyn Fn(u32, u32) -> Option<String>,
    limit: usize,
    out: Vec<String>,
    size: usize,
    stack: Vec<(String, usize)>,
    truncated: bool,
    elements: usize,
}

impl<'a> Writer<'a> {
    fn line(&mut self, depth: usize, text: &str) -> bool {
        if self.truncated {
            return false;
        }
        if self.size > self.limit {
            self.truncated = true;
            return false;
        }
        self.out.push(format!("{}{}", "  ".repeat(depth), text));
        self.size += 2 * depth + text.chars().count() + 1;
        true
    }

    fn sname(&self, si: u32) -> String {
        if (si as usize) < self.d.n_struct {
            self.d.struct_name(si as usize).unwrap_or("?").to_string()
        } else {
            format!("?{}", si)
        }
    }

    fn value(&self, t: u16, v: &Value) -> String {
        match (t, v) {
            (T_BOOL, Value::Bool(b)) => (if *b { "true" } else { "false" }).into(),
            (T_F32, Value::Float(f)) => fmt_float(*f, true),
            (T_F64, Value::Float(f)) => fmt_float(*f, false),
            (T_REF, Value::Ref(_, g)) => {
                if g == NULL_GUID {
                    "null".into()
                } else {
                    (self.ref_name)(g).unwrap_or_else(|| g.clone())
                }
            }
            (T_WEAK, Value::Weak(s, i)) => {
                if *s == NULL_PTR {
                    "null".into()
                } else {
                    (self.weak_name)(*s, *i).unwrap_or_else(|| format!("@{}[{}]", self.sname(*s), i))
                }
            }
            (T_STRONG, Value::Strong(s, i)) => {
                if *s == NULL_PTR {
                    "null".into()
                } else {
                    format!("@{}[{}]", self.sname(*s), i)
                }
            }
            (_, Value::Int(i)) => i.to_string(),
            (_, Value::UInt(u)) => u.to_string(),
            (_, Value::Bool(b)) => (if *b { "True" } else { "False" }).into(),
            (_, Value::Str(s)) | (_, Value::Guid(s)) => s.clone(),
            (_, Value::Float(f)) => py_repr(*f),
            _ => String::new(),
        }
    }

    /// One struct instance at absolute offset `off` as an element `tag`.
    fn instance(&mut self, tag: &str, si: usize, off: usize, depth: usize, extra: &str) -> Result<()> {
        let d = self.d;
        self.elements += 1;
        let lay = d.prop_layout(si)?;
        let mut attrs = String::new();
        let mut complex = vec![];
        for l in lay {
            let (t, c, po) = (l.t, l.c, l.off);
            if c != CONV_SINGLE || t == T_CLASS || t == T_STRONG {
                complex.push(l.clone());
                if !(c == CONV_SINGLE && t == T_STRONG) {
                    continue;
                }
                let (a, _b) = (u32_at(d.bytes(), off + po)?, u32_at(d.bytes(), off + po + 4)?);
                if a == NULL_PTR {
                    attrs.push_str(&format!(" {}=\"null\"", xml_name(&l.name)));
                }
                continue;
            }
            let v = d.read_inline(t, off + po)?;
            attrs.push_str(&format!(" {}=\"{}\"", xml_name(&l.name), esc_attr(&self.value(t, &v))));
        }
        let head = format!("<{}{}{}", tag, extra, attrs);
        if !self.line(depth, &format!("{}>", head)) {
            return Ok(());
        }
        let mark = self.out.len() - 1;
        self.stack.push((tag.to_string(), depth));
        if depth < MAX_DEPTH {
            for l in &complex {
                if self.truncated {
                    break;
                }
                self.member(&l.name, l.s as usize, l.t, l.c, off + l.off, depth + 1)?;
            }
        } else {
            self.line(depth + 1, "<!-- nesting too deep -->");
        }
        if self.truncated {
            return Ok(()); // keep the open elements on the stack: record_xml closes them
        }
        self.stack.pop();
        if self.out.len() == mark + 1 {
            let l = &mut self.out[mark];
            l.pop();
            l.push_str("/>");
        } else if !self.truncated {
            self.line(depth, &format!("</{}>", tag));
        }
        Ok(())
    }

    fn member(&mut self, nm: &str, s: usize, t: u16, c: u16, off: usize, depth: usize) -> Result<()> {
        let d = self.d;
        let tag = xml_name(nm);
        if c == CONV_SINGLE {
            if t == T_CLASS {
                return self.instance(&tag, s, off, depth, "");
            }
            // strong pointer (the only complex single left)
            let (a, b) = (u32_at(d.bytes(), off)?, u32_at(d.bytes(), off + 4)?);
            if a != NULL_PTR {
                let extra = format!(" __type=\"{}\" __idx=\"{}\"", esc_attr(&self.sname(a)), b);
                self.instance(&tag, a as usize, d.instance_offset(a as usize, b as usize)?, depth, &extra)?;
            }
            return Ok(());
        }
        let (cnt, first) = d.array_header(off)?;
        if cnt == 0 {
            self.line(depth, &format!("<{}/>", tag));
            return Ok(());
        }
        if !self.line(depth, &format!("<{}>", tag)) {
            return Ok(());
        }
        self.stack.push((tag.clone(), depth));
        if t == T_CLASS {
            for k in 0..cnt {
                if self.truncated {
                    break;
                }
                self.instance(&xml_name(&self.sname(s as u32)), s, d.instance_offset(s, first + k)?, depth + 1, "")?;
            }
        } else if t == T_STRONG {
            for k in 0..cnt {
                if self.truncated {
                    break;
                }
                let Value::Strong(a, b) = d.array_items(T_STRONG, first + k, 1)?.remove(0) else { unreachable!() };
                if a == NULL_PTR {
                    self.line(depth + 1, "<null/>");
                } else {
                    let extra = format!(" __type=\"{}\" __idx=\"{}\"", esc_attr(&self.sname(a)), b);
                    self.instance(&xml_name(&self.sname(a)), a as usize, d.instance_offset(a as usize, b as usize)?, depth + 1, &extra)?;
                }
            }
        } else {
            let ename = type_name(t).unwrap_or("Value");
            for v in d.array_items(t, first, cnt)? {
                let text = format!("<{}>{}</{}>", ename, esc_text(&self.value(t, &v)), ename);
                if !self.line(depth + 1, &text) {
                    break;
                }
            }
        }
        if self.truncated {
            return Ok(());
        }
        self.stack.pop();
        self.line(depth, &format!("</{}>", tag));
        Ok(())
    }
}

/// (xml text, info) for record `i` of reader `d`.  `ref_name(guid text) -> record name | None` and `weak_name(struct index, instance
/// index) -> record name | None` name the targets of references and weak pointers; `limit` caps the number of characters.
pub fn record_xml(d: &Dcb, i: usize, ref_name: Option<&dyn Fn(&str) -> Option<String>>, weak_name: Option<&dyn Fn(u32, u32) -> Option<String>>, limit: usize) -> Result<(String, XmlInfo)> {
    let none_ref = |_: &str| None;
    let none_weak = |_: u32, _: u32| None;
    let rec = d.record(i)?;
    let mut w = Writer { d, ref_name: ref_name.unwrap_or(&none_ref), weak_name: weak_name.unwrap_or(&none_weak), limit, out: vec![], size: 0, stack: vec![], truncated: false, elements: 0 };
    w.out.push("<?xml version=\"1.0\" encoding=\"utf-8\"?>".into());
    let extra = format!(" __type=\"{}\" __ref=\"{}\" __path=\"{}\"", esc_attr(&rec.struct_name), rec.guid, esc_attr(&rec.file));
    let off = d.instance_offset(rec.struct_idx as usize, rec.instance as usize)?;
    w.instance(&xml_name(&rec.name), rec.struct_idx as usize, off, 0, &extra)?;
    if w.truncated {
        w.out.push(format!("<!-- output cut at {} characters; the record is larger: raise the limit to see more -->", w.limit));
        for (tag, depth) in w.stack.iter().rev() {
            w.out.push(format!("{}</{}>", "  ".repeat(*depth), tag));
        }
    }
    let text = format!("{}\n", w.out.join("\n"));
    let info = XmlInfo { truncated: w.truncated, elements: w.elements, chars: text.chars().count() };
    Ok((text, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_escapes() {
        assert_eq!(xml_name("a b"), "a_b");
        assert_eq!(xml_name("1x"), "_1x");
        assert_eq!(xml_name(""), "_");
        assert_eq!(esc_attr("a<\"&\n"), "a&lt;&quot;&amp;&#10;");
    }

    #[test]
    fn float_text_matches_python() {
        assert_eq!(fmt_float(0.5, true), "0.5");
        assert_eq!(fmt_float(0.1f32 as f64, true), "0.1");
        assert_eq!(fmt_float(1e-5f32 as f64, true), "1e-05");
        assert_eq!(fmt_float(1234567.0, true), "1234567");
        assert_eq!(fmt_float(16777216.0, true), "16777216");
        assert_eq!(fmt_float(1.0, false), "1.0");
        assert_eq!(fmt_float(1e16, false), "1e+16");
        assert_eq!(fmt_float(1.5e-7, false), "1.5e-07");
        assert_eq!(fmt_float(123456789.0, false), "123456789.0");
        assert_eq!(fmt_float(f64::NAN, true), "nan");
    }
}
