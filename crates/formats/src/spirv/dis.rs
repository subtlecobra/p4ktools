//! SPIR-V disassembly in the spirv-dis style with friendly names, driven entirely by the grammar tables.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use super::table::{Operand, GLSL_STD_450};
use super::{debug_names, enum_kind, enum_name, enumerant, inst_info, string_at, Ins, Module};

struct Dis {
    names: HashMap<u32, String>,
    /// id of each scalar type -> (is float, width); constants print by their type
    scalar: HashMap<u32, (bool, u32)>,
    glsl_sets: HashSet<u32>,
}

impl Dis {
    fn id(&self, id: u32) -> String {
        match self.names.get(&id) {
            Some(n) => format!("%{}", n),
            None => format!("%{}", id),
        }
    }

    /// Prints the operands from word `p` on according to `spec`; returns the next word.
    fn operands(&self, ins: &Ins, spec: &[Operand], mut p: usize, out: &mut String) -> usize {
        for &(kind, quant) in spec {
            let mut first = true;
            while p < ins.o.len() && (first || quant == 2) {
                first = false;
                p = self.operand(ins, kind, p, out);
            }
        }
        p
    }

    fn operand(&self, ins: &Ins, kind: &str, mut p: usize, out: &mut String) -> usize {
        let o = ins.o;
        match kind {
            "IdResultType" | "IdResult" => p + 1,
            "IdRef" | "IdScope" | "IdMemorySemantics" => {
                let _ = write!(out, " {}", self.id(o[p]));
                p + 1
            }
            "LiteralString" => {
                let (s, n) = string_at(&o[p..]);
                let _ = write!(out, " {:?}", s);
                p + n
            }
            "LiteralExtInstInteger" => {
                let set = o.get(2).copied().unwrap_or(0);
                match GLSL_STD_450.binary_search_by_key(&o[p], |g| g.0) {
                    Ok(k) if self.glsl_sets.contains(&set) => {
                        let _ = write!(out, " {}", GLSL_STD_450[k].1);
                    }
                    _ => {
                        let _ = write!(out, " {}", o[p]);
                    }
                }
                p + 1
            }
            "LiteralSpecConstantOpInteger" => {
                let _ = write!(out, " {}", inst_info(o[p] as u16).map_or("?", |i| i.name));
                p + 1
            }
            "LiteralContextDependentNumber" => {
                let (float, width) = self.scalar.get(&o[0]).copied().unwrap_or((false, 32));
                let n = if width > 32 { 2 } else { 1 };
                let lo = o[p] as u64;
                let bits = if n == 2 { lo | (o.get(p + 1).copied().unwrap_or(0) as u64) << 32 } else { lo };
                let _ = match (float, width) {
                    (true, 64) => write!(out, " {:?}", f64::from_bits(bits)),
                    (true, 32) => write!(out, " {:?}", f32::from_bits(bits as u32)),
                    (true, _) => write!(out, " {:?}", super::hlsl::half(bits as u16)),
                    _ => write!(out, " {}", bits),
                };
                p + n
            }
            "PairLiteralIntegerIdRef" => {
                let _ = write!(out, " {}", o[p]);
                if let Some(t) = o.get(p + 1) {
                    let _ = write!(out, " {}", self.id(*t));
                }
                p + 2
            }
            "PairIdRefLiteralInteger" => {
                let _ = write!(out, " {}", self.id(o[p]));
                if let Some(t) = o.get(p + 1) {
                    let _ = write!(out, " {}", t);
                }
                p + 2
            }
            "PairIdRefIdRef" => {
                let _ = write!(out, " {}", self.id(o[p]));
                if let Some(t) = o.get(p + 1) {
                    let _ = write!(out, " {}", self.id(*t));
                }
                p + 2
            }
            _ => match enum_kind(kind) {
                Some(e) if e.bits => {
                    let v = o[p];
                    p += 1;
                    let set: Vec<_> = e.values.iter().filter(|x| x.value != 0 && v & x.value == x.value).collect();
                    let text = if v == 0 || set.is_empty() { enum_name(kind, v) } else { set.iter().map(|x| x.name).collect::<Vec<_>>().join("|") };
                    let _ = write!(out, " {}", text);
                    for x in set {
                        p = self.operands(ins, x.params, p, out);
                    }
                    p
                }
                Some(_) => {
                    let v = o[p];
                    let _ = write!(out, " {}", enum_name(kind, v));
                    p += 1;
                    match enumerant(kind, v) {
                        Some(x) => self.operands(ins, x.params, p, out),
                        None => p,
                    }
                }
                None => {
                    let _ = write!(out, " {}", o[p]);
                    p + 1
                }
            },
        }
    }
}

pub fn disassemble(m: &Module) -> String {
    let mut d = Dis { names: HashMap::new(), scalar: HashMap::new(), glsl_sets: HashSet::new() };
    // ids print by name; a name given to several ids gets its id appended so every line stays unambiguous
    let mut seen: HashMap<String, u32> = HashMap::new();
    let raw = debug_names(m);
    let mut ids: Vec<_> = raw.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let clean: String = raw[&id].chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '.' { c } else { '_' }).collect();
        let n = seen.entry(clean.clone()).or_insert(0);
        *n += 1;
        d.names.insert(id, if *n == 1 { clean } else { format!("{}_{}", clean, id) });
    }
    for i in m.iter() {
        match i.name {
            "OpTypeFloat" if i.o.len() >= 2 => {
                d.scalar.insert(i.o[0], (true, i.o[1]));
            }
            "OpTypeInt" if i.o.len() >= 2 => {
                d.scalar.insert(i.o[0], (false, i.o[1]));
            }
            "OpExtInstImport" if !i.o.is_empty() && string_at(&i.o[1..]).0 == "GLSL.std.450" => {
                d.glsl_sets.insert(i.o[0]);
            }
            _ => {}
        }
    }
    let mut out = String::with_capacity(m.len() * 40);
    let _ = writeln!(out, "; SPIR-V {}.{}\n; Generator: {:#x}\n; Bound: {}", (m.version >> 16) & 0xff, (m.version >> 8) & 0xff, m.generator, m.bound);
    for i in m.iter() {
        let Some(info) = i.info else {
            let _ = writeln!(out, "       OpUnknown({}) {:x?}", i.op, i.o);
            continue;
        };
        let has_type = info.operands.first().is_some_and(|o| o.0 == "IdResultType");
        let at = if has_type { 1 } else { 0 };
        let has_res = info.operands.get(at).is_some_and(|o| o.0 == "IdResult");
        if has_res {
            if let Some(r) = i.o.get(at) {
                let _ = write!(out, "{} = ", d.id(*r));
            }
        } else {
            out.push_str("       ");
        }
        out.push_str(i.name);
        if has_type {
            if let Some(t) = i.o.first() {
                let _ = write!(out, " {}", d.id(*t));
            }
        }
        d.operands(&i, info.operands, 0, &mut out);
        out.push('\n');
    }
    out
}
