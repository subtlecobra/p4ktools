//! SPIR-V reader, disassembler and decompiler for the Vulkan shader cache (`Engine/ShaderCache_Vulkan.pak`).
//!
//! EMPIRICAL (600 random entries of the live cache): every module is SPIR-V 1.5, generator 14 (the registry's "Google spiregg", the
//! HLSL front end of DXC), source language HLSL, logical addressing, GLSL450 memory model, with `OpName` debug names kept.  That is
//! why the decompiler (`hlsl.rs`) writes HLSL-like text and follows DXC's conventions.
//!
//! The opcode and enum tables (`table.rs`) are generated from the Khronos grammar, so nothing in them comes from memory.  Decoding
//! never panics on bad input: a damaged module is an `Err`, an instruction the decompiler does not know becomes a visible call and a warning.

mod dis;
mod hlsl;
pub mod table;

use std::collections::{HashMap, HashSet};

use table::{EnumKind, Enumerant, Inst, ENUMS, INSTS};

pub const MAGIC: u32 = 0x0723_0203;

/// One decoded shader; the same shape as `dxbc::DxbcShader` so the page treats both alike.
pub struct SpirvShader {
    /// "vs", "ps", "cs", "gs", "hs", "ds"
    pub stage: String,
    /// e.g. "SPIR-V 1.5"
    pub model: String,
    pub disassembly: String,
    pub hlsl: String,
    pub warnings: Vec<String>,
}

/// `data` starts at the SPIR-V magic.
pub fn decode(data: &[u8]) -> Result<SpirvShader, String> {
    let m = Module::parse(data)?;
    let stage = m.stage();
    let model = format!("SPIR-V {}.{}", (m.version >> 16) & 0xff, (m.version >> 8) & 0xff);
    let disassembly = dis::disassemble(&m);
    let (hlsl, warnings) = hlsl::decompile(&m);
    Ok(SpirvShader { stage, model, disassembly, hlsl, warnings })
}

/// One instruction: `o` is everything after the opcode word.
pub(crate) struct Ins<'m> {
    pub op: u16,
    pub name: &'static str,
    pub info: Option<&'static Inst>,
    pub o: &'m [u32],
}

pub(crate) struct Module {
    pub version: u32,
    pub generator: u32,
    pub bound: u32,
    words: Vec<u32>,
    /// (opcode, first word index, word count) of each instruction
    index: Vec<(u16, u32, u32)>,
}

impl Module {
    pub fn parse(data: &[u8]) -> Result<Module, String> {
        if data.len() < 20 {
            return Err("too short for a SPIR-V header".into());
        }
        let words: Vec<u32> = data.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        if words[0] != MAGIC {
            return Err(format!("not SPIR-V: magic {:08x}", words[0]));
        }
        let mut index = Vec::with_capacity(words.len() / 4);
        let mut i = 5usize;
        while i < words.len() {
            let n = (words[i] >> 16) as usize;
            if n == 0 || i + n > words.len() {
                return Err(format!("bad instruction length {} at word {}", n, i));
            }
            index.push(((words[i] & 0xffff) as u16, i as u32, n as u32));
            i += n;
        }
        Ok(Module { version: words[1], generator: words[2], bound: words[3], words, index })
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn ins(&self, k: usize) -> Ins<'_> {
        let (op, at, n) = self.index[k];
        let info = inst_info(op);
        Ins { op, name: info.map_or("OpUnknown", |i| i.name), info, o: &self.words[at as usize + 1..(at + n) as usize] }
    }

    pub fn iter(&self) -> impl Iterator<Item = Ins<'_>> {
        (0..self.len()).map(|k| self.ins(k))
    }

    fn stage(&self) -> String {
        let model = self.iter().find(|i| i.name == "OpEntryPoint").and_then(|i| i.o.first().copied());
        match model {
            Some(0) => "vs",
            Some(1) => "hs",
            Some(2) => "ds",
            Some(3) => "gs",
            Some(4) => "ps",
            Some(5) => "cs",
            _ => "?",
        }
        .to_string()
    }
}

pub(crate) fn inst_info(op: u16) -> Option<&'static Inst> {
    INSTS.binary_search_by_key(&op, |i| i.op).ok().map(|k| &INSTS[k])
}

pub(crate) fn enum_kind(kind: &str) -> Option<&'static EnumKind> {
    ENUMS.iter().find(|e| e.kind == kind)
}

pub(crate) fn enumerant(kind: &str, value: u32) -> Option<&'static Enumerant> {
    let e = enum_kind(kind)?;
    e.values.binary_search_by_key(&value, |v| v.value).ok().map(|k| &e.values[k])
}

pub(crate) fn enum_name(kind: &str, value: u32) -> String {
    enumerant(kind, value).map_or_else(|| format!("{}({})", kind, value), |e| e.name.to_string())
}

/// A nul-terminated string starting at `w[0]`: (text, words used).
pub(crate) fn string_at(w: &[u32]) -> (String, usize) {
    let mut b = Vec::new();
    for (k, x) in w.iter().enumerate() {
        for s in x.to_le_bytes() {
            if s == 0 {
                return (String::from_utf8_lossy(&b).into_owned(), k + 1);
            }
            b.push(s);
        }
    }
    (String::from_utf8_lossy(&b).into_owned(), w.len())
}

/// The `OpName`s of a module (id -> text as written).
pub(crate) fn debug_names(m: &Module) -> HashMap<u32, String> {
    m.iter().filter(|i| i.name == "OpName" && !i.o.is_empty()).map(|i| (i.o[0], string_at(&i.o[1..]).0)).filter(|(_, s)| !s.is_empty()).collect()
}

/// Names that the text uses outside the braces where they are declared.  The structuring of a decompiler can go wrong silently, in a
/// way that still reads well, so the corpus survey runs this over its output.  Only compiler names (`_123`) are tracked.
pub fn scope_errors(text: &str) -> Vec<String> {
    let mut scopes: Vec<HashSet<&str>> = vec![HashSet::new()];
    let mut bad = vec![];
    for line in text.lines() {
        let code = line.split("//").next().unwrap_or("");
        let closes = code.trim_start().starts_with('}');
        if closes && scopes.len() > 1 {
            scopes.pop();
        }
        let mut prev = "";
        let mut rest = code;
        while let Some(k) = rest.find(|c: char| c.is_alphanumeric() || c == '_') {
            let tail = &rest[k..];
            let n = tail.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(tail.len());
            let word = &tail[..n];
            let spaced = k > 0 && rest[..k].chars().all(|c| c == ' ');
            let member = k > 0 && rest[..k].ends_with('.');
            let compiler = word.len() > 1 && word.starts_with('_') && word[1..].chars().all(|c| c.is_ascii_digit());
            if compiler && !member {
                let declares = spaced && !prev.is_empty() && !matches!(prev, "return" | "case" | "goto");
                if declares {
                    scopes.last_mut().unwrap().insert(word);
                } else if !scopes.iter().any(|s| s.contains(word)) {
                    bad.push(word.to_string());
                }
            }
            prev = word;
            rest = &tail[n..];
        }
        if code.trim_end().ends_with('{') {
            scopes.push(HashSet::new());
        }
    }
    bad
}

#[cfg(test)]
mod tests;
