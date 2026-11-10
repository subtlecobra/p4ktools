//! ElementTree-style helpers over the shared `cryxml::Element` tree (`find("A/B")`, `findall`, `iter`).

use crate::cryxml::{self, Element};
use crate::error::*;
use crate::plainxml;

/// Root of a CryXmlB document or of a plain XML one.
pub fn parse_any(data: &[u8]) -> Result<Element> {
    if data.len() >= 7 && &data[..7] == b"CryXmlB" {
        cryxml::parse(data)
    } else {
        plainxml::parse(data)
    }
}

/// `findall("A/B/C")`: children named A of `e`, their B children, their C children (document order).
pub fn findall<'a>(e: &'a Element, path: &str) -> Vec<&'a Element> {
    let mut cur = vec![e];
    for step in path.split('/') {
        cur = cur.iter().flat_map(|x| x.children.iter().filter(move |c| c.tag == step)).collect();
    }
    cur
}

/// `find("A/B")`: first of `findall`.
pub fn find<'a>(e: &'a Element, path: &str) -> Option<&'a Element> {
    findall(e, path).into_iter().next()
}

/// `iter(tag)`: the element itself and every descendant with this tag, document order.
pub fn iter<'a>(e: &'a Element, tag: &str) -> Vec<&'a Element> {
    let mut out = Vec::new();
    e.iter(tag, &mut out);
    out
}

/// `Element.get(key) or ""`.
pub fn attr<'a>(e: &'a Element, key: &str) -> &'a str {
    e.get(key).unwrap_or("")
}

/// Split on NUL bytes like Python's `bytes.split(b"\0", maxsplit)`: at most `maxsplit + 1` pieces.
pub fn split_nul(b: &[u8], maxsplit: usize) -> Vec<&[u8]> {
    b.splitn(maxsplit.saturating_add(1), |&c| c == 0).collect()
}

/// latin-1 bytes to a String (every byte is the code point of the same value).
pub fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// CRC-32 (zlib polynomial) of `data`, what `zlib.crc32` returns.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = flate2::Crc::new();
    c.update(data);
    c.sum()
}

/// CRC32 of the string's UTF-8 bytes.
pub fn crc32_str(s: &str) -> u32 {
    crc32(s.as_bytes())
}
