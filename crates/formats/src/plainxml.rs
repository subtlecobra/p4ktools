//! Minimal plain-text XML reader producing the same `Element` tree as the CryXmlB reader.
//!
//! Why it exists: the metadata XML inside a `.socpak` is plain text in a large share of containers (EMPIRICAL: 560 of 1,455 sampled
//! non-editor metadata files; the rest are CryXmlB), and `read_child_containers` needs the `ChildObjectContainers` list of both.
//! Supported: prolog, comments, DOCTYPE (skipped), CDATA, the five named entities and numeric character references, self-closing
//! tags, single or double quoted attributes.  Namespaces are not interpreted.  Anything malformed is an error; callers skip
//! such a file.

use crate::cryxml::Element;
use crate::error::*;

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

fn is_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.') || b >= 0x80
}

fn unescape(raw: &str) -> Result<String> {
    if !raw.contains('&') {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let tail = &rest[p + 1..];
        let semi = tail.find(';').ok_or_else(|| Error::msg("ParseError: unterminated entity"))?;
        let ent = &tail[..semi];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse().ok()
                } else {
                    None
                };
                out.push(code.and_then(char::from_u32).ok_or_else(|| Error::msg("ParseError: undefined entity"))?);
            }
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl<'a> P<'a> {
    fn starts(&self, t: &[u8]) -> bool {
        self.s[self.i..].starts_with(t)
    }
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn skip_until(&mut self, t: &[u8]) -> Result<()> {
        match self.s[self.i..].windows(t.len()).position(|w| w == t) {
            Some(p) => {
                self.i += p + t.len();
                Ok(())
            }
            None => Err(Error::msg("ParseError: unclosed token")),
        }
    }
    fn name(&mut self) -> Result<String> {
        let st = self.i;
        while self.i < self.s.len() && is_name(self.s[self.i]) {
            self.i += 1;
        }
        if st == self.i {
            return Err(Error::msg("ParseError: name expected"));
        }
        Ok(String::from_utf8_lossy(&self.s[st..self.i]).into_owned())
    }
    /// Skips prolog items (declaration, comments, doctype, processing instructions) before an element or between tokens.
    fn misc(&mut self) -> Result<()> {
        loop {
            self.skip_ws();
            if self.starts(b"<?") {
                self.skip_until(b"?>")?;
            } else if self.starts(b"<!--") {
                self.skip_until(b"-->")?;
            } else if self.starts(b"<!DOCTYPE") {
                self.skip_until(b">")?;
            } else {
                return Ok(());
            }
        }
    }
    fn element(&mut self, depth: usize) -> Result<Element> {
        if depth > 2000 {
            return Err(Error::msg("RecursionError"));
        }
        if self.s.get(self.i) != Some(&b'<') {
            return Err(Error::msg("ParseError: element expected"));
        }
        self.i += 1;
        let tag = self.name()?;
        let mut el = Element { tag, ..Default::default() };
        loop {
            self.skip_ws();
            match self.s.get(self.i) {
                Some(b'/') => {
                    if self.s.get(self.i + 1) != Some(&b'>') {
                        return Err(Error::msg("ParseError: bad empty tag"));
                    }
                    self.i += 2;
                    return Ok(el);
                }
                Some(b'>') => {
                    self.i += 1;
                    break;
                }
                Some(_) => {
                    let k = self.name()?;
                    self.skip_ws();
                    if self.s.get(self.i) != Some(&b'=') {
                        return Err(Error::msg("ParseError: attribute value expected"));
                    }
                    self.i += 1;
                    self.skip_ws();
                    let q = *self.s.get(self.i).ok_or_else(|| Error::msg("ParseError: eof"))?;
                    if q != b'"' && q != b'\'' {
                        return Err(Error::msg("ParseError: unquoted attribute"));
                    }
                    self.i += 1;
                    let st = self.i;
                    while self.i < self.s.len() && self.s[self.i] != q {
                        self.i += 1;
                    }
                    if self.i >= self.s.len() {
                        return Err(Error::msg("ParseError: unterminated attribute"));
                    }
                    let v = unescape(&String::from_utf8_lossy(&self.s[st..self.i]))?;
                    self.i += 1;
                    if el.attrs.iter().any(|(ek, _)| *ek == k) {
                        return Err(Error::msg("ParseError: duplicate attribute"));
                    }
                    el.attrs.push((k, v));
                }
                None => return Err(Error::msg("ParseError: eof in tag")),
            }
        }
        // content
        loop {
            let st = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'<' {
                self.i += 1;
            }
            if self.i > st {
                el.text.push_str(&unescape(&String::from_utf8_lossy(&self.s[st..self.i]))?);
            }
            if self.i >= self.s.len() {
                return Err(Error::msg("ParseError: unclosed element"));
            }
            if self.starts(b"<!--") {
                self.skip_until(b"-->")?;
            } else if self.starts(b"<![CDATA[") {
                let st = self.i + 9;
                self.skip_until(b"]]>")?;
                el.text.push_str(&String::from_utf8_lossy(&self.s[st..self.i - 3]));
            } else if self.starts(b"<?") {
                self.skip_until(b"?>")?;
            } else if self.starts(b"</") {
                self.i += 2;
                let t = self.name()?;
                self.skip_ws();
                if t != el.tag || self.s.get(self.i) != Some(&b'>') {
                    return Err(Error::msg("ParseError: mismatched tag"));
                }
                self.i += 1;
                return Ok(el);
            } else {
                el.children.push(self.element(depth + 1)?);
            }
        }
    }
}

/// Root element of a plain XML document.
pub fn parse(data: &[u8]) -> Result<Element> {
    let mut p = P { s: data, i: 0 };
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        p.i = 3;
    }
    p.misc()?;
    let root = p.element(0)?;
    p.misc()?;
    if p.i != data.len() {
        return Err(Error::msg("ParseError: junk after document element"));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_document() {
        let e = parse(b"<?xml version=\"1.0\"?>\n<!-- c --><A x='1' y=\"a&amp;b\"><B/><C>t&#65;<![CDATA[<z>]]></C></A>\n").unwrap();
        assert_eq!(e.tag, "A");
        assert_eq!(e.get("y"), Some("a&b"));
        assert_eq!(e.children.len(), 2);
        assert_eq!(e.children[1].text, "tA<z>");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse(b"<A><B></A>").is_err());
        assert!(parse(b"<A x=1/>").is_err());
        assert!(parse(b"<A/><B/>").is_err());
    }
}
