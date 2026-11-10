//! D3D10/11 shader byte code (DXBC container, SM4/SM5 token stream): parser, fxc-style disassembler and an HLSL-like decompiler.
//!
//! Pure bytes in, text out: no archive access, no external tools. Entry point: [`decode`].
//!
//! Sources of knowledge (marks follow the repo convention):
//! - CONFIRMED against the public token format (`d3d11tokenizedprogramformat.h` layout: opcode numbers, operand and extended
//!   tokens, declaration payloads) and against Wine's reflection reader for RDEF; no engine addresses are involved.
//! - EMPIRICAL on the game's shader cache (`Engine/ShaderCache_D3D11.pak`, `.shcb` = 28-byte header + DXBC): the files carry
//!   RDEF, ISGN, OSGN (OSG5 for geometry shaders with streams), PCSG (hull shaders), SHEX and STAT, i.e. they are not stripped.
//!   See the corpus test at the bottom of this file for the measured figures.
//! - INFERRED: the HLSL output is a readable rendering of the byte code, not the original source. Semantics follow the
//!   instruction definitions; where a rendering is a pseudo-function (e.g. `ubfe`, `bfi`, `ld_structured`) it is named so.

mod disasm;
mod expr;
mod hlsl;
mod ops;
mod reflect;

pub use reflect::{CBuffer, CbVar, RType, Rdef, ResBinding, SigElem};

/// Result of [`decode`].
#[derive(Clone, Debug, Default)]
pub struct DxbcShader {
    /// "vs", "ps", "gs", "hs", "ds" or "cs".
    pub stage: String,
    /// Shader model, e.g. "5_0".
    pub model: String,
    pub disassembly: String,
    pub hlsl: String,
    pub warnings: Vec<String>,
}

/// Everything the printers need, assembled from the container.
pub(crate) struct Program {
    pub stage: u32,
    pub major: u32,
    pub minor: u32,
    pub instrs: Vec<ops::Instr>,
    pub rdef: Option<Rdef>,
    pub isg: Vec<SigElem>,
    pub osg: Vec<SigElem>,
    pub psg: Vec<SigElem>,
}

pub(crate) fn stage_name(s: u32) -> &'static str {
    match s {
        0 => "ps",
        1 => "vs",
        2 => "gs",
        3 => "hs",
        4 => "ds",
        5 => "cs",
        _ => "xx",
    }
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o.checked_add(4)?).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Chunk table: (fourcc, payload).
fn chunks<'a>(data: &'a [u8], warnings: &mut Vec<String>) -> Result<Vec<([u8; 4], &'a [u8])>, String> {
    if data.len() < 32 || &data[0..4] != b"DXBC" {
        return Err("not a DXBC container (missing magic)".into());
    }
    let n = u32_at(data, 28).ok_or("truncated header")? as usize;
    if n > 256 {
        return Err(format!("implausible chunk count {}", n));
    }
    let mut v = Vec::new();
    for i in 0..n {
        let Some(off) = u32_at(data, 32 + 4 * i) else {
            warnings.push("chunk offset table truncated".into());
            break;
        };
        let off = off as usize;
        let (Some(tag), Some(size)) = (data.get(off..off + 4), u32_at(data, off + 4)) else {
            warnings.push(format!("chunk {} header out of range", i));
            continue;
        };
        let start = off + 8;
        let mut end = start.saturating_add(size as usize);
        if end > data.len() {
            warnings.push(format!("chunk {} ({}) is truncated", i, String::from_utf8_lossy(tag)));
            end = data.len();
        }
        if start > end {
            continue;
        }
        v.push(([tag[0], tag[1], tag[2], tag[3]], &data[start..end]));
    }
    Ok(v)
}

fn sig_comment(title: &str, sig: &[SigElem]) -> String {
    let mut s = format!("//\n// {}\n//\n// Name                 Index   Mask Register SysValue  Format   Used\n", title);
    for e in sig {
        let fmt = match e.ctype {
            1 => "uint",
            2 => "int",
            3 => "float",
            _ => "?",
        };
        let m: String = (0..4).filter(|c| e.mask & (1 << c) != 0).map(|c| ['x', 'y', 'z', 'w'][c]).collect();
        let used: String = (0..4).filter(|c| e.rw & (1 << c) != 0).map(|c| ['x', 'y', 'z', 'w'][c]).collect();
        let reg = if e.reg == u32::MAX { "oDepth".to_string() } else { format!("{}", e.reg) };
        s.push_str(&format!(
            "// {:<20} {:>5}   {:<4} {:>8} {:<9} {:<7} {}\n",
            e.name,
            e.index,
            m,
            reg,
            if e.sysval == 0 { "NONE".to_string() } else { disasm::sysval_name(e.sysval).to_string() },
            fmt,
            used
        ));
    }
    s
}

fn rdef_comment(r: &Rdef) -> String {
    let mut s = String::new();
    if !r.creator.is_empty() {
        s.push_str(&format!("//\n// Generator: {}\n", r.creator));
    }
    for cb in &r.cbuffers {
        s.push_str(&format!("//\n// cbuffer {} // {} bytes\n// {{\n", cb.name, cb.size));
        for v in &cb.vars {
            s.push_str(&format!("//   {} {}; // Offset: {} Size: {}\n", hlsl::type_decl(&v.ty), v.name, v.offset, v.size));
        }
        s.push_str("// }\n");
    }
    if !r.resources.is_empty() {
        s.push_str("//\n// Resource bindings:\n//\n// Name                    Type  Format  Dim   Slot Elements\n");
        for b in &r.resources {
            s.push_str(&format!(
                "// {:<22} {:<6} {:<7} {:<5} {:>4} {:>8}\n",
                b.name,
                ["cbuffer", "tbuffer", "texture", "sampler", "UAV", "structured", "UAV_struct", "byteaddr", "UAV_byteaddr", "append", "consume", "counter"]
                    .get(b.kind as usize)
                    .unwrap_or(&"?"),
                disasm::ret_name(b.ret),
                ["NA", "buf", "1d", "1darr", "2d", "2darr", "2dms", "2dmsarr", "3d", "cube", "cubearr", "bufex"].get(b.dim as usize).unwrap_or(&"?"),
                b.bind_point,
                b.bind_count
            ));
        }
    }
    s
}

fn decode_inner(data: &[u8]) -> Result<DxbcShader, String> {
    let mut warnings = Vec::new();
    let ch = chunks(data, &mut warnings)?;
    let mut prog_chunk = None;
    let mut rdef = None;
    let (mut isg, mut osg, mut psg) = (vec![], vec![], vec![]);
    let mut names = Vec::new();
    for (tag, d) in &ch {
        names.push(String::from_utf8_lossy(tag).into_owned());
        match tag {
            b"SHEX" | b"SHDR" => {
                if prog_chunk.is_none() {
                    prog_chunk = Some(*d)
                }
            }
            b"RDEF" => match reflect::parse_rdef(d) {
                Ok(r) => rdef = Some(r),
                Err(e) => warnings.push(format!("RDEF: {}", e)),
            },
            b"ISGN" | b"ISG1" => match reflect::parse_sig(tag, d) {
                Ok(v) => isg = v,
                Err(e) => warnings.push(format!("{}: {}", String::from_utf8_lossy(tag), e)),
            },
            b"OSGN" | b"OSG1" | b"OSG5" => match reflect::parse_sig(tag, d) {
                Ok(v) => osg = v,
                Err(e) => warnings.push(format!("{}: {}", String::from_utf8_lossy(tag), e)),
            },
            b"PCSG" | b"PSG1" => match reflect::parse_sig(tag, d) {
                Ok(v) => psg = v,
                Err(e) => warnings.push(format!("{}: {}", String::from_utf8_lossy(tag), e)),
            },
            _ => {}
        }
    }
    let sh = prog_chunk.ok_or("no SHEX/SHDR chunk")?;
    let ver = u32_at(sh, 0).ok_or("shader chunk too short")?;
    let total = u32_at(sh, 4).ok_or("shader chunk too short")? as usize;
    let stage = ver >> 16;
    let (major, minor) = ((ver >> 4) & 0xf, ver & 0xf);
    let mut ndw = sh.len() / 4;
    if total >= 2 && total <= ndw {
        ndw = total;
    } else {
        warnings.push(format!("declared program length {} does not fit the chunk ({} dwords)", total, ndw));
    }
    let toks: Vec<u32> = sh.chunks_exact(4).take(ndw).skip(2).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let instrs = ops::decode_program(&toks, &mut warnings);
    let prog = Program { stage, major, minor, instrs, rdef, isg, osg, psg };

    let model = format!("{}_{}", major, minor);
    let mut header = String::new();
    header.push_str(&format!("// chunks: {}\n", names.join(" ")));
    if let Some(r) = &prog.rdef {
        header.push_str(&rdef_comment(r));
    }
    if !prog.isg.is_empty() {
        header.push_str(&sig_comment("Input signature:", &prog.isg));
    }
    if !prog.osg.is_empty() {
        header.push_str(&sig_comment("Output signature:", &prog.osg));
    }
    if !prog.psg.is_empty() {
        header.push_str(&sig_comment("Patch constant signature:", &prog.psg));
    }
    header.push_str(&format!("//\n{}_{}_{}", stage_name(stage), major, minor));
    let disassembly = disasm::disasm_program(&prog.instrs, &header);
    let hl = hlsl::decompile(&prog, &mut warnings);
    Ok(DxbcShader { stage: stage_name(stage).to_string(), model, disassembly, hlsl: hl, warnings })
}

/// Decode a DXBC blob (`data` starts at the "DXBC" magic). Never panics: internal failures come back as `Err`.
pub fn decode(data: &[u8]) -> Result<DxbcShader, String> {
    match std::panic::catch_unwind(|| decode_inner(data)) {
        Ok(r) => r,
        Err(_) => Err("internal error (panic) while decoding".into()),
    }
}

#[cfg(test)]
mod blobs;
#[cfg(test)]
mod tests;
