//! The shader caches as a library: `Engine/ShaderCache_{Vulkan,D3D11}.pak` opened in place and decoded on demand.
//!
//! EMPIRICAL: the three paks are stored uncompressed in Data.p4k (method 0), so each is a plain zip inside the memory map and reading
//! one shader decompresses only that entry; nothing of the 4.4 GB is copied.  `load` indexes the zip directories (359,000 entries).

use std::collections::{BTreeMap, HashMap};

use p4k_formats::shcb::{self, ExtFlag, ShaderName};
use p4k_formats::socpak::SocZip;
use p4k_formats::{dxbc, spirv};
use p4k::P4k;

pub const APIS: [&str; 2] = ["vulkan", "d3d11"];
const EXT_PAK: &str = "Engine/ShaderCache_EXT.pak";

/// One compiled shader of a cache.
pub struct Item {
    /// path inside the pak
    pub path: String,
    /// VShaders -> "vs" ...
    pub stage: &'static str,
    pub name: ShaderName,
    pub size: u64,
}

pub struct Pak {
    pub api: &'static str,
    pub pak_name: String,
    zip: SocZip<'static>,
    /// sorted by (shader, entry, gl, rt)
    pub items: Vec<Item>,
    by_path: HashMap<String, usize>,
    /// `Shaders/CFXIB/<API>/*.cfib|cfxb`, sizes
    pub fxb: Vec<(String, u64)>,
}

pub struct ShaderLib {
    pub paks: Vec<Pak>,
    /// lower-case shader name -> flags of its .ext
    pub ext: HashMap<String, Vec<ExtFlag>>,
    /// shader name -> .ext text, as stored
    pub ext_text: BTreeMap<String, String>,
    pub runtime: Vec<ExtFlag>,
}

/// A shader's build in the other cache; the diffs are the flag bits that differ when it is not exact.
pub struct Twin {
    pub api: &'static str,
    pub path: String,
    pub exact: bool,
    pub gl_diff: u64,
    pub rt_diff: u64,
}

/// The candidate of the same stage and entry point with the fewest differing flag bits; GL bits count first because they change the
/// material code, then RT bits, then the other name parts.  Ties go to the first candidate, so the result is stable.
fn closest<'a>(item: &Item, family: &'a [Item]) -> Option<&'a Item> {
    family.iter().filter(|c| c.stage == item.stage && c.name.entry == item.name.entry).min_by_key(|c| {
        ((c.name.gl ^ item.name.gl).count_ones(), (c.name.rt ^ item.name.rt).count_ones(), (c.name.rest != item.name.rest) as u32)
    })
}

/// What the page shows for one shader.
pub struct Decoded {
    pub header: shcb::Shcb,
    pub stage: String,
    pub model: String,
    pub disassembly: String,
    pub hlsl: String,
    pub warnings: Vec<String>,
}

fn stage_of(dir: &str) -> &'static str {
    match dir {
        "VShaders" => "vs",
        "PShaders" => "ps",
        "CShaders" => "cs",
        "GShaders" => "gs",
        "HShaders" => "hs",
        "DShaders" => "ds",
        _ => "?",
    }
}

impl Pak {
    fn open(p4k: &'static P4k, api: &'static str, file: &str) -> Result<Pak, String> {
        let ent = p4k.get(&format!("Engine/{}", file)).ok_or_else(|| format!("Engine/{} is not in the archive", file))?;
        if ent.method != 0 {
            return Err(format!("Engine/{} is compressed in the archive (expected stored)", file));
        }
        let data: &'static [u8] = p4k.raw(ent).map_err(|e| e.to_string())?;
        let zip = SocZip::open(data).map_err(|e| e.to_string())?;
        let (mut items, mut fxb) = (vec![], vec![]);
        for (path, size) in zip.sizes() {
            if path.ends_with('/') {
                continue;
            }
            if path.ends_with(".shcb") {
                let mut parts = path.split('/');
                let dir = parts.nth(2).unwrap_or("");
                let base = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".shcb");
                let name = shcb::parse_name(base).unwrap_or_else(|| ShaderName { shader: "(unnamed)".to_string(), entry: base.to_string(), ..Default::default() });
                items.push(Item { path: path.to_string(), stage: stage_of(dir), name, size });
            } else {
                fxb.push((path.to_string(), size));
            }
        }
        items.sort_by(|a, b| (&a.name.shader, &a.name.entry, a.name.gl, a.name.rt, &a.path).cmp(&(&b.name.shader, &b.name.entry, b.name.gl, b.name.rt, &b.path)));
        fxb.sort();
        let by_path = items.iter().enumerate().map(|(k, i)| (i.path.clone(), k)).collect();
        Ok(Pak { api, pak_name: file.to_string(), zip, items, by_path, fxb })
    }

    pub fn item(&self, path: &str) -> Option<&Item> {
        self.by_path.get(path).map(|k| &self.items[*k])
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        self.zip.read(path).map_err(|e| e.to_string())
    }

    /// The range of `items` holding one shader (they are sorted by name).
    pub fn family(&self, shader: &str) -> &[Item] {
        let a = self.items.partition_point(|i| i.name.shader.as_str() < shader);
        let b = self.items.partition_point(|i| i.name.shader.as_str() <= shader);
        &self.items[a..b]
    }

    /// One shader's bytecode decoded.
    pub fn decode(&self, path: &str) -> Result<Decoded, String> {
        let raw = self.read(path)?;
        decode_entry(self.api, &raw)
    }
}

/// Decodes a whole `.shcb` entry.
pub fn decode_entry(api: &str, raw: &[u8]) -> Result<Decoded, String> {
    let (header, code) = shcb::parse(raw)?;
    if api == "vulkan" {
        let s = spirv::decode(code)?;
        return Ok(Decoded { header, stage: s.stage, model: s.model, disassembly: s.disassembly, hlsl: s.hlsl, warnings: s.warnings });
    }
    let s = dxbc::decode(code)?;
    Ok(Decoded { header, stage: s.stage, model: s.model, disassembly: s.disassembly, hlsl: s.hlsl, warnings: s.warnings })
}

impl ShaderLib {
    pub fn load(p4k: &'static P4k) -> Result<ShaderLib, String> {
        let paks = vec![Pak::open(p4k, "vulkan", "ShaderCache_Vulkan.pak")?, Pak::open(p4k, "d3d11", "ShaderCache_D3D11.pak")?];
        let (mut ext, mut ext_text, mut runtime) = (HashMap::new(), BTreeMap::new(), vec![]);
        if let Some(ent) = p4k.get(EXT_PAK) {
            let data: &'static [u8] = p4k.raw(ent).map_err(|e| e.to_string())?;
            if ent.method == 0 {
                let zip = SocZip::open(data).map_err(|e| e.to_string())?;
                let names: Vec<String> = zip.namelist().map(str::to_string).collect();
                for n in names {
                    let Some(base) = n.strip_prefix("Shaders/").and_then(|b| b.strip_suffix(".ext")) else { continue };
                    let bytes = zip.read(&n).map_err(|e| e.to_string())?;
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    let flags = shcb::parse_ext(&text);
                    if base.eq_ignore_ascii_case("RunTime") {
                        runtime = flags.clone();
                    }
                    ext.insert(base.to_lowercase(), flags);
                    ext_text.insert(base.to_string(), text);
                }
            }
        }
        Ok(ShaderLib { paks, ext, ext_text, runtime })
    }

    pub fn pak(&self, api: &str) -> Option<&Pak> {
        self.paks.iter().find(|p| p.api == api)
    }

    /// The same shader in the other cache.  EMPIRICAL: the caches hold different permutation sets (87,251 paths are in both, 60,203 only
    /// in Vulkan, 124,234 only in D3D11), so when there is no exact twin this is the closest build of the same entry point.
    pub fn counterpart(&self, api: &str, path: &str) -> Option<Twin> {
        let other = self.paks.iter().find(|p| p.api != api)?;
        let this = self.pak(api)?;
        let item = this.item(path)?;
        let folder = |p: &Pak| if p.api == "vulkan" { "Shaders/Vulkan/" } else { "Shaders/D3D11/" };
        let exact = format!("{}{}", folder(other), path.strip_prefix(folder(this))?);
        if other.item(&exact).is_some() {
            return Some(Twin { api: other.api, path: exact, exact: true, gl_diff: 0, rt_diff: 0 });
        }
        let near = closest(item, other.family(&item.name.shader))?;
        Some(Twin { api: other.api, path: near.path.clone(), exact: false, gl_diff: near.name.gl ^ item.name.gl, rt_diff: near.name.rt ^ item.name.rt })
    }

    pub fn gl_flags(&self, shader: &str) -> &[ExtFlag] {
        self.ext.get(&shader.to_lowercase()).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(entry: &str, stage: &'static str, gl: u64, rt: u64, rest: &[&str]) -> Item {
        let name = ShaderName { shader: "S".into(), entry: entry.into(), gl, rt, rest: rest.iter().map(|s| s.to_string()).collect() };
        Item { path: format!("{}{:x}{:x}{}", entry, gl, rt, rest.len()), stage, name, size: 0 }
    }

    #[test]
    fn closest_keeps_the_entry_point_and_prefers_matching_material_bits() {
        let want = item("A", "ps", 0x401, 0x20, &[]);
        let family = [item("B", "ps", 0x401, 0x20, &[]), item("A", "vs", 0x401, 0x20, &[]), item("A", "ps", 0x400, 0x20, &[]), item("A", "ps", 0x401, 0x7, &["MDV1"]), item("A", "ps", 0x401, 0x21, &["MDV1"])];
        assert_eq!(closest(&want, &family).unwrap().path, family[4].path);
        assert!(closest(&item("C", "ps", 0, 0, &[]), &family).is_none());
        // the same flags win over a matching suffix
        let f2 = [item("A", "ps", 0x401, 0x20, &["MDV2"]), item("A", "ps", 0x401, 0x21, &[])];
        assert_eq!(closest(&want, &f2).unwrap().path, f2[0].path);
    }

    #[test]
    fn stages() {
        assert_eq!(stage_of("PShaders"), "ps");
        assert_eq!(stage_of("Other"), "?");
    }
}
