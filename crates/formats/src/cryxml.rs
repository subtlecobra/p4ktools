//! Reader for CIG's CryEngine binary XML ("CryXmlB"), ported from the scdatatools reader.
//!
//! References:
//!   - Lumberyard XMLBinaryHeaders.h / XMLBinaryReader.cpp
//!   - dolkensp/unp4k CryXmlSerializer.cs
//!
//! Layout (EMPIRICAL, checked on real files, identical to the scdatatools reader): header 44 bytes = signature "CryXmlB\0" (8) then
//! u32 xml_size, node_table_offset, node_count, attributes_table_offset, attributes_count, child_table_offset, child_table_count,
//! string_data_offset, string_data_size.  A node is 28 bytes: u32 tag_string_offset, u32 content_string_offset, u16
//! attribute_count, u16 child_count, u32 parent_index, u32 first_attribute_index, u32 first_child_index and 4 reserved bytes (CIG's
//! XMLBinary::Node carries an extra u32).  An attribute is two u32 string offsets; the child table is u32 node indices.  Strings
//! are NUL terminated in the string block.  A repeated attribute key keeps its first position and its last value (dict semantics).
//!
//! Not supported: plain-text XML input (files starting with '<'); callers get an error.

use crate::error::*;

pub const NO_PARENT: u32 = 0xFFFF_FFFF;
const HEADER: usize = 44;
const NODE: usize = 28;

#[derive(Debug, Clone, Default)]
pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// First child with this tag.
    pub fn find(&self, tag: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.tag == tag)
    }

    /// All descendants (and self) with this tag, document order.
    pub fn iter<'a>(&'a self, tag: &str, out: &mut Vec<&'a Element>) {
        if self.tag == tag {
            out.push(self);
        }
        for c in &self.children {
            c.iter(tag, out);
        }
    }
}

pub fn is_cryxmlb(data: &[u8]) -> bool {
    data.len() >= HEADER && &data[..7] == b"CryXmlB"
}

struct Reader<'a> {
    d: &'a [u8],
    str_off: usize,
    str_size: usize,
    node_off: usize,
    attr_off: usize,
    child_off: usize,
    n_nodes: usize,
    n_attrs: usize,
    n_child: usize,
}

impl<'a> Reader<'a> {
    fn string(&self, offset: u32) -> Result<String> {
        let start = self.str_off + offset as usize;
        let limit = (self.str_off + self.str_size).min(self.d.len());
        let hay = self.d.get(start..limit).ok_or_else(|| Error::msg("ValueError: string offset out of range"))?;
        let end = hay.iter().position(|&b| b == 0).ok_or_else(|| Error::msg("ValueError: subsection not found"))?;
        std::str::from_utf8(&hay[..end]).map(str::to_owned).map_err(|e| Error::msg(format!("UnicodeDecodeError: {}", e)))
    }

    fn node(&self, index: usize, depth: usize) -> Result<Element> {
        if index >= self.n_nodes {
            return Err(Error::msg("node index out of range"));
        }
        if depth > 10_000 {
            return Err(Error::msg("RecursionError"));
        }
        let o = self.node_off + index * NODE;
        let tag = self.string(u32_at(self.d, o)?)?;
        let content = self.string(u32_at(self.d, o + 4)?)?;
        let attr_count = u16_at(self.d, o + 8)? as usize;
        let child_count = u16_at(self.d, o + 10)? as usize;
        let first_attr = u32_at(self.d, o + 16)? as usize;
        let first_child = u32_at(self.d, o + 20)? as usize;
        let mut attrs: Vec<(String, String)> = Vec::with_capacity(attr_count);
        for i in first_attr..first_attr + attr_count {
            if i >= self.n_attrs {
                return Err(Error::msg("attribute index out of range"));
            }
            let ao = self.attr_off + i * 8;
            let k = self.string(u32_at(self.d, ao)?)?;
            let v = self.string(u32_at(self.d, ao + 4)?)?;
            match attrs.iter_mut().find(|(ek, _)| *ek == k) {
                Some(slot) => slot.1 = v,
                None => attrs.push((k, v)),
            }
        }
        let mut children = Vec::with_capacity(child_count);
        for i in first_child..first_child + child_count {
            if i >= self.n_child {
                return Err(Error::msg("child index out of range"));
            }
            let ci = u32_at(self.d, self.child_off + 4 * i)? as usize;
            children.push(self.node(ci, depth + 1)?);
        }
        Ok(Element { tag, attrs, text: content, children })
    }
}

/// The root element of a CryXmlB document.
pub fn parse(data: &[u8]) -> Result<Element> {
    if data.len() < HEADER {
        return Err(Error::msg("ValueError: File is not a binary XML file (file size is too small)."));
    }
    if &data[..7] != b"CryXmlB" || data[7] != 0 {
        if data[0] == b'<' {
            return Err(Error::msg("plain-text XML is not supported by the Rust reader"));
        }
        return Err(Error::msg("ParseError: Invalid CryXmlB Signature"));
    }
    let h = |i: usize| u32_at(data, 8 + 4 * i).map(|v| v as usize);
    let r = Reader {
        d: data,
        node_off: h(1)?,
        n_nodes: h(2)?,
        attr_off: h(3)?,
        n_attrs: h(4)?,
        child_off: h(5)?,
        n_child: h(6)?,
        str_off: h(7)?,
        str_size: h(8)?,
    };
    // a corrupt table fails because the walk touches what it needs; unreferenced garbage is not an error
    if r.n_nodes == 0 {
        return Err(Error::msg("node 0 missing"));
    }
    if u32_at(data, r.node_off + 12)? != NO_PARENT {
        return Err(Error::msg("AssertionError: root node has a parent"));
    }
    r.node(0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built document: <Root a="1"><Kid b="2">text</Kid></Root>
    fn doc() -> Vec<u8> {
        let strings = b"Root\0a\x001\0Kid\0b\x002\0text\0\0".to_vec(); // offsets: Root 0, a 5, 1 7, Kid 9, b 13, 2 15, text 17, "" 22
        let node = |tag: u32, content: u32, na: u16, nc: u16, parent: u32, fa: u32, fc: u32| {
            let mut v = vec![];
            v.extend(tag.to_le_bytes());
            v.extend(content.to_le_bytes());
            v.extend(na.to_le_bytes());
            v.extend(nc.to_le_bytes());
            v.extend(parent.to_le_bytes());
            v.extend(fa.to_le_bytes());
            v.extend(fc.to_le_bytes());
            v.extend([0u8; 4]);
            v
        };
        let nodes = [node(0, 22, 1, 1, NO_PARENT, 0, 0), node(9, 17, 1, 0, 0, 1, 1)].concat();
        let attrs: Vec<u8> = [5u32, 7, 13, 15].iter().flat_map(|v| v.to_le_bytes()).collect();
        let child: Vec<u8> = 1u32.to_le_bytes().to_vec();
        let mut hdr = b"CryXmlB\0".to_vec();
        let nt = HEADER as u32;
        let at = nt + nodes.len() as u32;
        let ct = at + attrs.len() as u32;
        let st = ct + child.len() as u32;
        for v in [0u32, nt, 2, at, 2, ct, 1, st, strings.len() as u32] {
            hdr.extend(v.to_le_bytes());
        }
        [hdr, nodes, attrs, child, strings].concat()
    }

    #[test]
    fn parses_tree() {
        let e = parse(&doc()).unwrap();
        assert_eq!(e.tag, "Root");
        assert_eq!(e.get("a"), Some("1"));
        assert_eq!(e.children[0].tag, "Kid");
        assert_eq!(e.children[0].get("b"), Some("2"));
        assert_eq!(e.children[0].text, "text");
    }

    #[test]
    fn rejects_bad_signature() {
        let mut d = doc();
        d[0] = b'X';
        assert!(parse(&d).is_err());
        assert!(parse(&d[..10]).is_err());
    }
}
