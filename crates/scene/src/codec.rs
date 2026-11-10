//! A small binary codec for the data a disk cache stores: LEB128 varints, length-prefixed strings and a
//! lossless `serde_json::Value` encoding.  Our own format on purpose (never a pickle, never code): `Reader` returns `None` on any
//! truncated or inconsistent input, so a damaged cache file is a miss.
//!
//! Why not JSON text: parsing tens of MB of JSON costs about as much as some of the builds we cache; this is exact (i64 / u64 / f64
//! stay distinct, object key order is kept) and cheaper to read (EMPIRICAL, benchmarked).

use serde_json::{Map, Number, Value};

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn uv(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    pub fn iv(&mut self, v: i64) {
        self.uv(((v << 1) ^ (v >> 63)) as u64);
    }

    pub fn f64(&mut self, v: f64) {
        self.buf.extend(v.to_le_bytes());
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.uv(b.len() as u64);
        self.buf.extend(b);
    }

    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    pub fn opt_str(&mut self, s: &Option<String>) {
        match s {
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
            None => self.u8(0),
        }
    }

    pub fn opt_f64(&mut self, v: Option<f64>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.f64(v);
            }
            None => self.u8(0),
        }
    }

    pub fn strs(&mut self, v: &[String]) {
        self.uv(v.len() as u64);
        for s in v {
            self.str(s);
        }
    }

    pub fn u32s(&mut self, v: &[u32]) {
        self.uv(v.len() as u64);
        for &x in v {
            self.uv(x as u64);
        }
    }

    /// Lossless `Value` encoding: tags 0 null, 1 false, 2 true, 3 i64, 4 u64, 5 f64, 6 string, 7 array, 8 object.
    pub fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.u8(0),
            Value::Bool(false) => self.u8(1),
            Value::Bool(true) => self.u8(2),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    self.u8(3);
                    self.iv(i);
                } else if let Some(u) = n.as_u64() {
                    self.u8(4);
                    self.uv(u);
                } else {
                    self.u8(5);
                    self.f64(n.as_f64().unwrap_or(0.0));
                }
            }
            Value::String(s) => {
                self.u8(6);
                self.str(s);
            }
            Value::Array(a) => {
                self.u8(7);
                self.uv(a.len() as u64);
                for x in a {
                    self.value(x);
                }
            }
            Value::Object(o) => {
                self.u8(8);
                self.uv(o.len() as u64);
                for (k, x) in o {
                    self.str(k);
                    self.value(x);
                }
            }
        }
    }
}

pub struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Reader<'a> {
        Reader { b, p: 0 }
    }

    pub fn done(&self) -> bool {
        self.p == self.b.len()
    }

    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }

    pub fn uv(&mut self) -> Option<u64> {
        let (mut v, mut sh) = (0u64, 0u32);
        loop {
            let b = self.u8()?;
            if sh > 63 {
                return None;
            }
            v |= ((b & 0x7f) as u64) << sh;
            if b < 0x80 {
                return Some(v);
            }
            sh += 7;
        }
    }

    pub fn iv(&mut self) -> Option<i64> {
        let u = self.uv()?;
        Some(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    /// A count that cannot exceed the bytes left (each element takes at least one byte): guards `Vec::with_capacity` against garbage.
    pub fn count(&mut self) -> Option<usize> {
        let n = self.uv()? as usize;
        (n <= self.b.len() - self.p).then_some(n)
    }

    pub fn f64(&mut self) -> Option<f64> {
        let s = self.b.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(f64::from_le_bytes(s.try_into().ok()?))
    }

    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.uv()? as usize;
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }

    pub fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes()?).ok()
    }

    pub fn string(&mut self) -> Option<String> {
        self.str().map(str::to_string)
    }

    pub fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.string()?)),
            _ => None,
        }
    }

    pub fn opt_f64(&mut self) -> Option<Option<f64>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.f64()?)),
            _ => None,
        }
    }

    pub fn strs(&mut self) -> Option<Vec<String>> {
        let n = self.count()?;
        (0..n).map(|_| self.string()).collect()
    }

    pub fn u32s(&mut self) -> Option<Vec<u32>> {
        let n = self.count()?;
        (0..n).map(|_| self.uv().map(|x| x as u32)).collect()
    }

    pub fn value(&mut self) -> Option<Value> {
        self.value_depth(0)
    }

    fn value_depth(&mut self, depth: usize) -> Option<Value> {
        if depth > 200 {
            return None;
        }
        Some(match self.u8()? {
            0 => Value::Null,
            1 => Value::Bool(false),
            2 => Value::Bool(true),
            3 => Value::Number(Number::from(self.iv()?)),
            4 => Value::Number(Number::from(self.uv()?)),
            5 => Value::Number(Number::from_f64(self.f64()?)?),
            6 => Value::String(self.string()?),
            7 => {
                let n = self.count()?;
                let mut a = Vec::with_capacity(n);
                for _ in 0..n {
                    a.push(self.value_depth(depth + 1)?);
                }
                Value::Array(a)
            }
            8 => {
                let n = self.count()?;
                let mut o = Map::new();
                for _ in 0..n {
                    let k = self.string()?;
                    o.insert(k, self.value_depth(depth + 1)?);
                }
                Value::Object(o)
            }
            _ => return None,
        })
    }
}

/// `values` as one blob (count, then each value).
pub fn encode_values(values: &[Value]) -> Vec<u8> {
    let mut w = Writer::new();
    w.uv(values.len() as u64);
    for v in values {
        w.value(v);
    }
    w.buf
}

pub fn decode_values(b: &[u8]) -> Option<Vec<Value>> {
    let mut r = Reader::new(b);
    let n = r.count()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.value()?);
    }
    r.done().then_some(out)
}

pub fn encode_value(v: &Value) -> Vec<u8> {
    let mut w = Writer::new();
    w.value(v);
    w.buf
}

pub fn decode_value(b: &[u8]) -> Option<Value> {
    let mut r = Reader::new(b);
    let v = r.value()?;
    r.done().then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn varints_round_trip() {
        let mut w = Writer::new();
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            w.uv(v);
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, -300] {
            w.iv(v);
        }
        let mut r = Reader::new(&w.buf);
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            assert_eq!(r.uv(), Some(v));
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, -300] {
            assert_eq!(r.iv(), Some(v));
        }
        assert!(r.done());
    }

    #[test]
    fn values_round_trip_exactly() {
        let v = json!({"b": [1, -2, 3.5, 1.0, null, true, false, "x\u{e9}"], "a": {"z": u64::MAX, "y": i64::MIN, "w": 0.1}, "": []});
        let back = decode_value(&encode_value(&v)).unwrap();
        assert_eq!(back, v);
        assert_eq!(serde_json::to_string(&back).unwrap(), serde_json::to_string(&v).unwrap(), "key order and number kinds survive");
        assert!(back["b"][3].is_f64() && back["b"][0].is_i64());
    }

    #[test]
    fn damaged_input_is_none() {
        let v = json!({"k": [1, 2, 3, "abc"]});
        let b = encode_value(&v);
        for n in 0..b.len() {
            assert!(decode_value(&b[..n]).is_none(), "truncated at {}", n);
        }
        assert!(decode_value(&[9]).is_none());
        assert!(decode_value(&[7, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_none(), "huge count");
        let mut extra = b.clone();
        extra.push(0);
        assert!(decode_value(&extra).is_none(), "trailing bytes");
    }
}
