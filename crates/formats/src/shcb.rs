//! Compiled shader cache entries (`.shcb`), their file names, and the `.ext` flag tables that give the permutation bits their names.
//!
//! Layout, EMPIRICAL (600 random Vulkan entries, plus D3D11 spot checks; no engine address yet):
//!   +0  'SHCB'
//!   +4  u32 tag, be 22 b2 00 on every entry and also in the FXB1 files of the same cache (a cache or format version; meaning not known)
//!   +8  8 bytes, differ per entry (probably a hash; not verified)
//!   +16 u32 file size, equal to the entry's size on all 600
//!   +20 u32, small values 0,1,2,3,5 (meaning not known)
//!   +24 u32, varies (meaning not known)
//!   +28 the bytecode: SPIR-V in ShaderCache_Vulkan.pak, a DXBC container in ShaderCache_D3D11.pak
//!
//! Names, EMPIRICAL: `Shaders/<API>/<Stage>Shaders/<Shader>@<Entry>(GL<hex>)(RT<hex>)...(out(PS)).shcb`; GL is absent when zero on some
//! entries.  GL bits are the shader's own `.ext` Property masks (e.g. Illum BLENDLAYER = 0x100 in `Illum.ext`) and RT
//! bits are those of `RunTime.ext`.

pub const HEADER: usize = 28;

#[derive(Debug, Clone, PartialEq)]
pub struct Shcb {
    pub tag: u32,
    pub hash: u64,
    pub size: u32,
    pub unknown20: u32,
    pub unknown24: u32,
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// The header and the bytecode behind it.
pub fn parse(d: &[u8]) -> Result<(Shcb, &[u8]), String> {
    if d.len() < HEADER || &d[..4] != b"SHCB" {
        return Err("not a SHCB entry".to_string());
    }
    let hash = u64::from_le_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]);
    Ok((Shcb { tag: u32_at(d, 4), hash, size: u32_at(d, 16), unknown20: u32_at(d, 20), unknown24: u32_at(d, 24) }, &d[HEADER..]))
}

/// A cache entry's file name taken apart.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShaderName {
    pub shader: String,
    pub entry: String,
    pub gl: u64,
    pub rt: u64,
    /// the parenthesised parts other than GL and RT, as written
    pub rest: Vec<String>,
}

/// `HardSurface@HardSurfacePS(GL401)(RTa20000000)` (the base name, without `.shcb`).  None when it does not have the `shader@entry` form.
pub fn parse_name(base: &str) -> Option<ShaderName> {
    let (shader, tail) = base.split_once('@')?;
    let open = tail.find('(').unwrap_or(tail.len());
    let mut n = ShaderName { shader: shader.to_string(), entry: tail[..open].to_string(), ..Default::default() };
    // top-level groups only: out(PS) nests
    let (mut depth, mut start) = (0usize, 0usize);
    for (k, c) in tail.char_indices().skip_while(|x| x.0 < open) {
        match c {
            '(' => {
                if depth == 0 {
                    start = k + 1;
                }
                depth += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let g = &tail[start..k];
                    let hex = |p: &str| g.strip_prefix(p).and_then(|h| u64::from_str_radix(h, 16).ok());
                    if let Some(v) = hex("GL") {
                        n.gl = v;
                    } else if let Some(v) = hex("RT") {
                        n.rt = v;
                    } else {
                        n.rest.push(g.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    Some(n)
}

/// One `Property` of an `.ext` file.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtFlag {
    pub name: String,
    pub mask: u64,
    pub description: String,
}

/// The `Property { Name = %X  Mask = 0x1 ... Description (text) }` blocks of an `.ext` file, in file order.
pub fn parse_ext(text: &str) -> Vec<ExtFlag> {
    let mut out = vec![];
    let mut rest = text;
    while let Some(k) = rest.find("Property") {
        let after = &rest[k + "Property".len()..];
        let Some(open) = after.find(|c: char| !c.is_whitespace()).filter(|p| after[*p..].starts_with('{')) else {
            rest = after;
            continue;
        };
        let Some(end) = after[open..].find('}') else { break };
        let body = &after[open + 1..open + end];
        let (mut name, mut mask, mut description) = (None, None, String::new());
        for line in body.lines().map(str::trim) {
            if let Some(v) = line.strip_prefix("Name").and_then(|l| l.trim_start().strip_prefix('=')) {
                name = Some(v.trim().trim_start_matches('%').to_string());
            } else if let Some(v) = line.strip_prefix("Mask").and_then(|l| l.trim_start().strip_prefix('=')) {
                let v = v.trim();
                mask = v.strip_prefix("0x").map_or(v.parse().ok(), |h| u64::from_str_radix(h, 16).ok());
            } else if let Some(v) = line.strip_prefix("Description") {
                description = v.trim().trim_start_matches('(').trim_end_matches(')').to_string();
            }
        }
        if let (Some(name), Some(mask)) = (name, mask) {
            out.push(ExtFlag { name, mask, description });
        }
        rest = &after[open + end..];
    }
    out
}

/// The names of the flags set in `bits`, and the bits no flag explains.  Multi-bit masks count only when every bit is set.
pub fn flag_names(flags: &[ExtFlag], bits: u64) -> (Vec<String>, u64) {
    let mut left = bits;
    let mut names = vec![];
    for f in flags {
        if f.mask != 0 && bits & f.mask == f.mask {
            names.push(f.name.clone());
            left &= !f.mask;
        }
    }
    (names, left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header() {
        let mut d = b"SHCB".to_vec();
        d.extend([0xbe, 0x22, 0xb2, 0]);
        d.extend(7u64.to_le_bytes());
        d.extend(32u32.to_le_bytes());
        d.extend(1u32.to_le_bytes());
        d.extend(9u32.to_le_bytes());
        d.extend([1, 2, 3, 4]);
        let (h, body) = parse(&d).unwrap();
        assert_eq!((h.hash, h.size, h.unknown20, h.unknown24), (7, 32, 1, 9));
        assert_eq!(body, [1, 2, 3, 4]);
        assert!(parse(b"nope").is_err());
    }

    #[test]
    fn names() {
        let n = parse_name("WaterVolume@WV_WaterBufferProducerVS(GL6)(RT0)(out(PS))").unwrap();
        assert_eq!((n.shader.as_str(), n.entry.as_str(), n.gl, n.rt), ("WaterVolume", "WV_WaterBufferProducerVS", 6, 0));
        assert_eq!(n.rest, ["out(PS)"]);
        let n = parse_name("HardSurface@HardSurfacePS(GL401)(RTa20000000)").unwrap();
        assert_eq!((n.gl, n.rt), (0x401, 0xa_2000_0000));
        assert_eq!(parse_name("AuxGeom@AuxGeomLineVS").unwrap().entry, "AuxGeomLineVS");
        assert!(parse_name("no_at_sign").is_none());
    }

    #[test]
    fn ext() {
        let t = "// c\nVersion (1.00)\n\nProperty\n{\n  Name = %NORMAL_MAP\n  Mask = 0x1\n  Property    (Normal map)\n  Description (Normal map for glass)\n  Hidden\n}\n\nProperty\n{\n\tName = %_RT_QUALITY\n\tMask = 0x8000000000\n}\n";
        let f = parse_ext(t);
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].name.as_str(), f[0].mask, f[0].description.as_str()), ("NORMAL_MAP", 1, "Normal map for glass"));
        assert_eq!(f[1].mask, 0x80_0000_0000);
        assert_eq!(flag_names(&f, 3), (vec!["NORMAL_MAP".to_string()], 2));
    }
}
