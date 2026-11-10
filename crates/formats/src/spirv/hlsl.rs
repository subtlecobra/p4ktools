//! SPIR-V -> readable HLSL-like text.
//!
//! How it works: types, constants, decorations and globals are collected first.  Each function is then written by walking its blocks
//! from the entry, using the merge instructions that shader SPIR-V must carry to rebuild `if` / `while` / `switch`.
//!
//! * Values: an id used once in its own block is folded into its user; anything else becomes a local.  A folded read of writable
//!   memory is written out before the next statement with side effects, so reads never move across writes.
//! * `OpPhi` becomes a variable declared at the top of the function and assigned on each incoming edge.
//! * A `continue` block is inlined at each edge that reaches it, because a `continue;` statement in `while (true)` would skip it.
//! * A local that turns out to be used outside the braces it was declared in is hoisted: the body is written again with the
//!   declaration at the top of the function (`hoist`).  Jumps with no structured spelling become `goto` plus a label.
//!
//! The text is for reading: it follows the module's semantics but is not guaranteed to compile.  Known simplifications: the matrix
//! order below; `!=` for `OpFOrdNotEqual`, which differs on NaN only.
//!
//! Matrices, INFERRED from DXC's documented convention: an HLSL row-major `floatRxC` is emitted as a SPIR-V matrix of R columns, so a
//! SPIR-V "column" is an HLSL row and the operands of a product are swapped.  `mul()` here is written in the original HLSL order.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use super::table::GLSL_STD_450;
use super::{debug_names, enum_name, string_at, Ins, Module};

const ATOM: u8 = 100;
const UNARY: u8 = 90;
const MUL: u8 = 80;
const ADD: u8 = 70;
const SHIFT: u8 = 65;
const CMP: u8 = 60;
const EQ: u8 = 55;
const BAND: u8 = 50;
const BXOR: u8 = 48;
const BOR: u8 = 46;
const LAND: u8 = 44;
const LOR: u8 = 42;
const TERN: u8 = 40;

const MAX_DEPTH: usize = 160;

// decoration numbers (SPIR-V spec, Decoration)
const D_BLOCK: u32 = 2;
const D_BUFFER_BLOCK: u32 = 3;
const D_BUILTIN: u32 = 11;
const D_NOPERSPECTIVE: u32 = 13;
const D_FLAT: u32 = 14;
const D_CENTROID: u32 = 16;
const D_SAMPLE: u32 = 17;
const D_NONWRITABLE: u32 = 24;
const D_LOCATION: u32 = 30;
const D_BINDING: u32 = 33;
const D_SET: u32 = 34;
const D_OFFSET: u32 = 35;

// storage classes
const SC_UNIFORM_CONSTANT: u32 = 0;
const SC_INPUT: u32 = 1;
const SC_UNIFORM: u32 = 2;
const SC_OUTPUT: u32 = 3;
const SC_WORKGROUP: u32 = 4;
const SC_PRIVATE: u32 = 6;
const SC_FUNCTION: u32 = 7;
const SC_PUSH_CONSTANT: u32 = 9;
const SC_STORAGE_BUFFER: u32 = 12;

pub fn half(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let e = ((h >> 10) & 31) as i32;
    let m = (h & 0x3ff) as f32;
    match e {
        0 => s * m * 2f32.powi(-24),
        31 if m == 0.0 => s * f32::INFINITY,
        31 => f32::NAN,
        _ => s * (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

#[derive(Clone, Debug)]
enum Ty {
    Void,
    Bool,
    Int(u32, bool),
    Float(u32),
    Vector(u32, u32),
    Matrix(u32, u32),
    /// sampled type, dim, arrayed, ms, sampled (1 texture, 2 storage)
    Image(u32, u32, u32, u32, u32),
    Sampler,
    SampledImage(u32),
    Array(u32, u32),
    RuntimeArray(u32),
    Struct(Vec<u32>),
    Pointer(u32),
    Other,
}

#[derive(Clone, Debug)]
enum Cv {
    Bool(bool),
    Bits(u64),
    Composite(Vec<u32>),
    Null,
    Undef,
}

/// An expression: its text, the precedence of its outermost operator, whether it reads writable memory, and for a plain name or a
/// swizzle of one the SSA id it stands for.
#[derive(Clone, Debug)]
struct E {
    t: String,
    p: u8,
    mem: bool,
    /// no arithmetic or calls: repeating the text costs nothing, so a multi-use value may stay an alias
    simple: bool,
    root: Option<u32>,
}

impl E {
    fn new(t: String, p: u8, mem: bool) -> E {
        E { t, p, mem, simple: false, root: None }
    }
    fn atom(t: String) -> E {
        E { t, p: ATOM, mem: false, simple: true, root: None }
    }
    fn wrap(&self, min: u8) -> String {
        if self.p < min {
            format!("({})", self.t)
        } else {
            self.t.clone()
        }
    }
}

#[derive(Clone)]
enum Frame {
    Sel { merge: u32 },
    Loop { header: u32, merge: u32, cont: u32 },
    Switch { merge: u32 },
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Dest {
    /// the end of the innermost selection: nothing to write
    Fall,
    Break,
    Continue,
    /// the loop's continue block: written in place
    ContBlock,
    Goto,
    Inline,
}

#[derive(Clone, Copy)]
enum Merge {
    None,
    Sel(u32),
    Loop(u32, u32),
}

struct Block {
    first: usize,
    /// index of the terminator
    term: usize,
    merge: Merge,
}

/// Facts about one function, collected before writing it.
#[derive(Default)]
struct Fun {
    id: u32,
    ret: u32,
    params: Vec<u32>,
    first_block: u32,
    blocks: HashMap<u32, Block>,
    uses: HashMap<u32, u32>,
    /// the single block that uses an id, or 0 when used in several or by a phi
    use_block: HashMap<u32, u32>,
    /// target block -> [(phi id, type, [(value, predecessor)])]
    phis: HashMap<u32, Vec<(u32, u32, Vec<(u32, u32)>)>>,
    phi_ids: HashSet<u32>,
    locals: Vec<usize>,
}

struct Gen<'m> {
    m: &'m Module,
    names: HashMap<u32, String>,
    taken: HashMap<String, u32>,
    member_names: HashMap<(u32, u32), String>,
    decos: HashMap<u32, Vec<(u32, Vec<u32>)>>,
    member_decos: HashMap<(u32, u32), Vec<(u32, Vec<u32>)>>,
    semantics: HashMap<u32, String>,
    types: HashMap<u32, Ty>,
    consts: HashMap<u32, (u32, Cv)>,
    /// type of every id that has one
    ty_of: HashMap<u32, u32>,
    globals: Vec<(u32, u32, u32, Option<u32>)>,
    /// structs that only wrap a runtime array (structured and byte address buffers): chains skip their member 0
    wrappers: HashSet<u32>,
    glsl: HashSet<u32>,
    warnings: Vec<String>,
    warned: HashSet<String>,
    // ---- per function run
    out: String,
    indent: usize,
    vals: HashMap<u32, E>,
    pending: Vec<u32>,
    sampled: HashMap<u32, (String, String)>,
    scope_of: HashMap<u32, usize>,
    open: Vec<usize>,
    next_scope: usize,
    hoist: HashSet<u32>,
    want_hoist: HashSet<u32>,
    labels: HashSet<u32>,
    want_labels: HashSet<u32>,
    frames: Vec<Frame>,
    steps: usize,
    max_steps: usize,
    depth: usize,
    cur: u32,
}

pub fn decompile(m: &Module) -> (String, Vec<String>) {
    let mut g = Gen::new(m);
    g.collect();
    let text = g.module();
    (text, g.warnings)
}

fn sanitize(s: &str) -> String {
    let mut o: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if o.is_empty() || o.as_bytes()[0].is_ascii_digit() {
        o.insert(0, '_');
    }
    o
}

impl<'m> Gen<'m> {
    fn new(m: &'m Module) -> Gen<'m> {
        Gen {
            m,
            names: HashMap::new(),
            taken: HashMap::new(),
            member_names: HashMap::new(),
            decos: HashMap::new(),
            member_decos: HashMap::new(),
            semantics: HashMap::new(),
            types: HashMap::new(),
            consts: HashMap::new(),
            ty_of: HashMap::new(),
            globals: vec![],
            wrappers: HashSet::new(),
            glsl: HashSet::new(),
            warnings: vec![],
            warned: HashSet::new(),
            out: String::new(),
            indent: 0,
            vals: HashMap::new(),
            pending: vec![],
            sampled: HashMap::new(),
            scope_of: HashMap::new(),
            open: vec![],
            next_scope: 0,
            hoist: HashSet::new(),
            want_hoist: HashSet::new(),
            labels: HashSet::new(),
            want_labels: HashSet::new(),
            frames: vec![],
            steps: 0,
            max_steps: 0,
            depth: 0,
            cur: 0,
        }
    }

    fn warn(&mut self, s: String) {
        if self.warned.insert(s.clone()) {
            self.warnings.push(s);
        }
    }

    // ------------------------------------------------------------------------------------------------------- module facts

    fn collect(&mut self) {
        let raw = debug_names(self.m);
        let mut ids: Vec<_> = raw.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let n = self.unique(&raw[&id], id);
            self.names.insert(id, n);
        }
        for i in self.m.iter() {
            let o = i.o;
            // every instruction below reads fixed leading operands: a short one is simply ignored
            match i.name {
                "OpMemberName" if o.len() >= 3 => {
                    self.member_names.insert((o[0], o[1]), sanitize(&string_at(&o[2..]).0));
                }
                "OpDecorate" | "OpDecorateId" if o.len() >= 2 => self.decos.entry(o[0]).or_default().push((o[1], o[2..].to_vec())),
                "OpMemberDecorate" if o.len() >= 3 => self.member_decos.entry((o[0], o[1])).or_default().push((o[2], o[3..].to_vec())),
                "OpDecorateString" if o.len() >= 3 => {
                    self.semantics.insert(o[0], string_at(&o[2..]).0);
                }
                "OpExtInstImport" if o.len() >= 2 => {
                    if string_at(&o[1..]).0 == "GLSL.std.450" {
                        self.glsl.insert(o[0]);
                    }
                }
                _ => self.type_or_const(&i),
            }
            if let Some(info) = i.info {
                if info.operands.first().is_some_and(|x| x.0 == "IdResultType") && o.len() >= 2 {
                    self.ty_of.insert(o[1], o[0]);
                }
            }
        }
        // unnamed built-in stage variables read better by what they are
        let globals: Vec<u32> = self.globals.iter().map(|g| g.0).collect();
        for id in globals {
            if let (false, Some(b)) = (self.names.contains_key(&id), self.deco(id, D_BUILTIN)) {
                let n = self.unique(&format!("gl_{}", enum_name("BuiltIn", b)), id);
                self.names.insert(id, n);
            }
        }
        // buffers: DXC's type.StructuredBuffer.X / type.ByteAddressBuffer wrap one runtime array
        let wrap: Vec<u32> = self
            .types
            .iter()
            .filter(|(id, t)| {
                matches!(t, Ty::Struct(ms) if ms.len() == 1 && matches!(self.types.get(&ms[0]), Some(Ty::RuntimeArray(_))))
                    && raw.get(id).is_some_and(|n| n.contains("Buffer"))
            })
            .map(|(id, _)| *id)
            .collect();
        self.wrappers.extend(wrap);
    }

    fn unique(&mut self, raw: &str, id: u32) -> String {
        let s = sanitize(raw);
        let n = self.taken.entry(s.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            s
        } else {
            format!("{}_{}", s, id)
        }
    }

    fn type_or_const(&mut self, i: &Ins) {
        let o = i.o;
        let at = |k: usize| o.get(k).copied().unwrap_or(0);
        let ty = match i.name {
            "OpTypeVoid" => Ty::Void,
            "OpTypeBool" => Ty::Bool,
            "OpTypeInt" => Ty::Int(at(1), at(2) != 0),
            "OpTypeFloat" => Ty::Float(at(1)),
            "OpTypeVector" => Ty::Vector(at(1), at(2)),
            "OpTypeMatrix" => Ty::Matrix(at(1), at(2)),
            "OpTypeImage" => Ty::Image(at(1), at(2), at(4), at(5), at(6)),
            "OpTypeSampler" => Ty::Sampler,
            "OpTypeSampledImage" => Ty::SampledImage(at(1)),
            "OpTypeArray" => Ty::Array(at(1), at(2)),
            "OpTypeRuntimeArray" => Ty::RuntimeArray(at(1)),
            "OpTypeStruct" => Ty::Struct(o.get(1..).unwrap_or(&[]).to_vec()),
            "OpTypePointer" => Ty::Pointer(at(2)),
            "OpTypeFunction" | "OpTypeForwardPointer" | "OpTypeOpaque" | "OpTypeAccelerationStructureKHR" => Ty::Other,
            "OpConstantTrue" | "OpSpecConstantTrue" => return self.constant(o, Cv::Bool(true)),
            "OpConstantFalse" | "OpSpecConstantFalse" => return self.constant(o, Cv::Bool(false)),
            "OpConstant" | "OpSpecConstant" => return self.constant(o, Cv::Bits(at(2) as u64 | (at(3) as u64) << 32)),
            "OpConstantComposite" | "OpSpecConstantComposite" => return self.constant(o, Cv::Composite(o.get(2..).unwrap_or(&[]).to_vec())),
            "OpConstantNull" => return self.constant(o, Cv::Null),
            "OpUndef" => return self.constant(o, Cv::Undef),
            "OpVariable" => {
                if at(2) != SC_FUNCTION && o.len() >= 3 {
                    self.globals.push((at(1), at(0), at(2), o.get(3).copied()));
                }
                return;
            }
            _ => return,
        };
        // a type may only refer to earlier types and is defined once, so a damaged module cannot build a cycle for `type_name` to chase
        let known = |t: &u32| self.types.contains_key(t);
        let ok = match &ty {
            Ty::Vector(e, _) | Ty::Matrix(e, _) | Ty::Array(e, _) | Ty::RuntimeArray(e) | Ty::SampledImage(e) | Ty::Pointer(e) => known(e),
            Ty::Image(e, ..) => known(e),
            Ty::Struct(ms) => ms.iter().all(known),
            _ => true,
        };
        if let Some(id) = o.first() {
            self.types.entry(*id).or_insert(if ok { ty } else { Ty::Other });
        }
    }

    fn constant(&mut self, o: &[u32], v: Cv) {
        if o.len() >= 2 {
            // parts must exist already, for the same reason as types
            let ok = !matches!(&v, Cv::Composite(ps) if ps.iter().any(|p| !self.consts.contains_key(p)));
            self.consts.entry(o[1]).or_insert((o[0], if ok { v } else { Cv::Undef }));
        }
    }

    fn ty(&self, id: u32) -> Ty {
        self.types.get(&id).cloned().unwrap_or(Ty::Other)
    }

    fn deco(&self, id: u32, d: u32) -> Option<u32> {
        self.decos.get(&id)?.iter().find(|x| x.0 == d).map(|x| x.1.first().copied().unwrap_or(0))
    }

    fn has_deco(&self, id: u32, d: u32) -> bool {
        self.deco(id, d).is_some()
    }

    fn name(&self, id: u32) -> String {
        self.names.get(&id).cloned().unwrap_or_else(|| format!("_{}", id))
    }

    fn pointee(&self, t: u32) -> u32 {
        match self.ty(t) {
            Ty::Pointer(p) => p,
            _ => t,
        }
    }

    fn type_name(&self, id: u32) -> String {
        match self.ty(id) {
            Ty::Void => "void".into(),
            Ty::Bool => "bool".into(),
            Ty::Int(16, true) => "int16_t".into(),
            Ty::Int(16, false) => "uint16_t".into(),
            Ty::Int(64, true) => "int64_t".into(),
            Ty::Int(64, false) => "uint64_t".into(),
            Ty::Int(_, true) => "int".into(),
            Ty::Int(_, false) => "uint".into(),
            Ty::Float(16) => "half".into(),
            Ty::Float(64) => "double".into(),
            Ty::Float(_) => "float".into(),
            Ty::Vector(e, n) => format!("{}{}", self.type_name(e), n),
            Ty::Matrix(c, n) => match self.ty(c) {
                Ty::Vector(e, r) => format!("{}{}x{}", self.type_name(e), n, r),
                _ => "matrix".into(),
            },
            Ty::Image(s, dim, arrayed, ms, sampled) => {
                let base = match dim {
                    0 => "Texture1D",
                    1 => "Texture2D",
                    2 => "Texture3D",
                    3 => "TextureCube",
                    5 => "Buffer",
                    6 => "SubpassInput",
                    _ => "Texture",
                };
                format!("{}{}{}{}<{}4>", if sampled == 2 { "RW" } else { "" }, base, if ms != 0 { "MS" } else { "" }, if arrayed != 0 { "Array" } else { "" }, self.type_name(s))
            }
            Ty::Sampler => "SamplerState".into(),
            Ty::SampledImage(i) => self.type_name(i),
            Ty::Array(e, _) | Ty::RuntimeArray(e) => self.type_name(e),
            Ty::Struct(_) => self.struct_name(id),
            Ty::Pointer(p) => self.type_name(p),
            Ty::Other => format!("type_{}", id),
        }
    }

    fn struct_name(&self, id: u32) -> String {
        let n = self.name(id);
        n.strip_prefix("type_").filter(|s| !s.is_empty()).map(str::to_string).unwrap_or(n)
    }

    /// `T name[N]`
    fn decl(&self, ty: u32, name: &str) -> String {
        let mut dims = String::new();
        let mut t = ty;
        loop {
            match self.ty(t) {
                Ty::Array(e, n) => {
                    let _ = match self.const_u32(n) {
                        Some(k) => write!(dims, "[{}]", k),
                        None => write!(dims, "[{}]", self.const_text(n).t),
                    };
                    t = e;
                }
                Ty::RuntimeArray(e) => {
                    dims.push_str("[]");
                    t = e;
                }
                Ty::Pointer(p) => t = p,
                _ => break,
            }
        }
        format!("{} {}{}", self.type_name(t), name, dims)
    }

    fn const_text(&self, id: u32) -> E {
        let Some((ty, v)) = self.consts.get(&id).cloned() else { return E::atom(self.name(id)) };
        match v {
            Cv::Bool(b) => E::atom(b.to_string()),
            Cv::Bits(bits) => self.scalar_text(ty, bits),
            Cv::Null => E::new(format!("({})0", self.type_name(ty)), UNARY, false),
            Cv::Undef => E::new(format!("({})0 /* undef */", self.type_name(ty)), UNARY, false),
            Cv::Composite(parts) => {
                let p: Vec<String> = parts.iter().map(|c| self.const_text(*c).t).collect();
                match self.ty(ty) {
                    Ty::Array(..) | Ty::Struct(_) => E::atom(format!("{{ {} }}", p.join(", "))),
                    Ty::Vector(..) if p.len() > 1 && p.iter().all(|x| *x == p[0]) => E::new(format!("({}){}", self.type_name(ty), p[0]), UNARY, false),
                    _ => E::atom(format!("{}({})", self.type_name(ty), p.join(", "))),
                }
            }
        }
    }

    fn scalar_text(&self, ty: u32, bits: u64) -> E {
        match self.ty(ty) {
            Ty::Float(w) => {
                let (s, raw) = match w {
                    64 => {
                        let f = f64::from_bits(bits);
                        (f.is_finite().then(|| format!("{:?}", f)), bits)
                    }
                    16 => {
                        let f = half(bits as u16);
                        (f.is_finite().then(|| format!("{:?}", f)), bits & 0xffff)
                    }
                    _ => {
                        let f = f32::from_bits(bits as u32);
                        (f.is_finite().then(|| format!("{:?}", f)), bits & 0xffff_ffff)
                    }
                };
                match s {
                    Some(t) if t.starts_with('-') => E { t, p: UNARY, mem: false, simple: true, root: None },
                    Some(t) => E::atom(t),
                    None => E::atom(format!("asfloat({:#x}u)", raw)),
                }
            }
            Ty::Int(w, true) => {
                let v = if w <= 32 { bits as u32 as i32 as i64 } else { bits as i64 };
                E { t: v.to_string(), p: if v < 0 { UNARY } else { ATOM }, mem: false, simple: true, root: None }
            }
            Ty::Int(w, false) => {
                let v = if w <= 32 { bits & 0xffff_ffff } else { bits };
                E::atom(if v >= 0x10000 { format!("{:#x}u", v) } else { format!("{}u", v) })
            }
            _ => E::atom(bits.to_string()),
        }
    }

    // ------------------------------------------------------------------------------------------------------- declarations

    fn module(&mut self) -> String {
        let mut head = String::new();
        let _ = writeln!(head, "// Decompiled from SPIR-V {}.{} (generator {:#x}); HLSL-like text for reading, not guaranteed to compile", (self.m.version >> 16) & 0xff, (self.m.version >> 8) & 0xff, self.m.generator);
        let mut entry = 0;
        let mut attrs = vec![];
        for i in self.m.iter() {
            let o = i.o;
            match i.name {
                "OpEntryPoint" if o.len() >= 3 => {
                    entry = o[1];
                    let _ = writeln!(head, "// {} entry point \"{}\"", enum_name("ExecutionModel", o[0]), string_at(&o[2..]).0);
                }
                "OpExecutionMode" if o.len() >= 2 => {
                    let mode = enum_name("ExecutionMode", o[1]);
                    if mode == "LocalSize" && o.len() >= 5 {
                        attrs.push(format!("[numthreads({}, {}, {})]", o[2], o[3], o[4]));
                    } else if mode == "EarlyFragmentTests" {
                        attrs.push("[earlydepthstencil]".to_string());
                    } else if mode != "OriginUpperLeft" {
                        let args: Vec<String> = o[2..].iter().map(u32::to_string).collect();
                        let _ = writeln!(head, "// execution mode {} {}", mode, args.join(" "));
                    }
                }
                _ => {}
            }
        }
        head.push('\n');
        self.declare_structs(&mut head);
        self.declare_globals(&mut head);
        let funs = self.functions();
        let (entries, helpers): (Vec<_>, Vec<_>) = funs.into_iter().partition(|f| f.id == entry);
        for f in helpers.iter().chain(entries.iter()) {
            if f.id == entry {
                for a in &attrs {
                    head.push_str(a);
                    head.push('\n');
                }
            }
            let text = self.function(f, f.id == entry);
            head.push_str(&text);
            head.push('\n');
        }
        head
    }

    fn is_buffer_struct(&self, id: u32) -> bool {
        self.has_deco(id, D_BLOCK) || self.has_deco(id, D_BUFFER_BLOCK) || self.wrappers.contains(&id)
    }

    fn declare_structs(&self, out: &mut String) {
        // declaration order keeps a struct after the structs it contains
        for i in self.m.iter() {
            if i.name != "OpTypeStruct" || i.o.is_empty() {
                continue;
            }
            let id = i.o[0];
            if self.is_buffer_struct(id) {
                continue;
            }
            let _ = writeln!(out, "struct {} {{", self.struct_name(id));
            self.members(id, out);
            out.push_str("};\n\n");
        }
    }

    fn member_name(&self, st: u32, k: u32) -> String {
        self.member_names.get(&(st, k)).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| format!("_m{}", k))
    }

    fn members(&self, id: u32, out: &mut String) {
        let Ty::Struct(ms) = self.ty(id) else { return };
        for (k, t) in ms.iter().enumerate() {
            let off = self.member_decos.get(&(id, k as u32)).and_then(|d| d.iter().find(|x| x.0 == D_OFFSET)).and_then(|x| x.1.first().copied());
            let _ = write!(out, "    {};", self.decl(*t, &self.member_name(id, k as u32)));
            if let Some(off) = off {
                let _ = write!(out, " // offset {}", off);
            }
            out.push('\n');
        }
    }

    fn binding_attr(&self, id: u32) -> String {
        match (self.deco(id, D_BINDING), self.deco(id, D_SET)) {
            (Some(b), Some(s)) => format!("[[vk::binding({}, {})]] ", b, s),
            (Some(b), None) => format!("[[vk::binding({})]] ", b),
            _ => String::new(),
        }
    }

    fn semantic(&self, id: u32) -> String {
        if let Some(b) = self.deco(id, D_BUILTIN) {
            let n = enum_name("BuiltIn", b);
            return match n.as_str() {
                "Position" | "FragCoord" => "SV_Position",
                "FragDepth" => "SV_Depth",
                "VertexIndex" => "SV_VertexID",
                "InstanceIndex" => "SV_InstanceID",
                "FrontFacing" => "SV_IsFrontFace",
                "GlobalInvocationId" => "SV_DispatchThreadID",
                "LocalInvocationId" => "SV_GroupThreadID",
                "WorkgroupId" => "SV_GroupID",
                "LocalInvocationIndex" => "SV_GroupIndex",
                "SampleId" => "SV_SampleIndex",
                "SampleMask" => "SV_Coverage",
                "ClipDistance" => "SV_ClipDistance",
                "CullDistance" => "SV_CullDistance",
                "Layer" => "SV_RenderTargetArrayIndex",
                "ViewportIndex" => "SV_ViewportArrayIndex",
                "TessCoord" => "SV_DomainLocation",
                "TessLevelOuter" => "SV_TessFactor",
                "TessLevelInner" => "SV_InsideTessFactor",
                "InvocationId" => "SV_OutputControlPointID",
                "PrimitiveId" => "SV_PrimitiveID",
                _ => return n,
            }
            .to_string();
        }
        if let Some(s) = self.semantics.get(&id) {
            return s.clone();
        }
        // DXC names stage variables in.var.TEXCOORD0 / out.var.SV_Target0 (EMPIRICAL)
        let raw = self.name(id);
        raw.strip_prefix("in_var_").or_else(|| raw.strip_prefix("out_var_")).unwrap_or("").to_string()
    }

    fn declare_globals(&self, out: &mut String) {
        let mut groups: [Vec<String>; 4] = Default::default();
        for &(id, ptr, class, init) in &self.globals {
            let pointee = self.pointee(ptr);
            let name = self.name(id);
            let bind = self.binding_attr(id);
            match class {
                SC_INPUT | SC_OUTPUT => {
                    let mut l = format!("{} {}", if class == SC_INPUT { "in" } else { "out" }, self.decl(ptr, &name));
                    let sem = self.semantic(id);
                    if !sem.is_empty() {
                        let _ = write!(l, " : {}", sem);
                    }
                    l.push(';');
                    let mut notes = vec![];
                    if let Some(loc) = self.deco(id, D_LOCATION) {
                        notes.push(format!("location {}", loc));
                    }
                    for (d, n) in [(D_FLAT, "nointerpolation"), (D_NOPERSPECTIVE, "noperspective"), (D_CENTROID, "centroid"), (D_SAMPLE, "sample")] {
                        if self.has_deco(id, d) {
                            notes.push(n.to_string());
                        }
                    }
                    if !notes.is_empty() {
                        let _ = write!(l, " // {}", notes.join(", "));
                    }
                    groups[0].push(l);
                }
                SC_UNIFORM | SC_STORAGE_BUFFER | SC_PUSH_CONSTANT if matches!(self.ty(pointee), Ty::Struct(_)) || self.array_of_struct(pointee).is_some() => {
                    let st = self.array_of_struct(pointee).unwrap_or(pointee);
                    if self.wrappers.contains(&st) {
                        let Ty::Struct(ms) = self.ty(st) else { continue };
                        let elem = match self.ty(ms[0]) {
                            Ty::RuntimeArray(e) => e,
                            _ => ms[0],
                        };
                        let raw = self.struct_name(st);
                        let kind = raw.split('_').next().unwrap_or("StructuredBuffer").to_string();
                        groups[1].push(format!("{}{}<{}> {};", bind, kind, self.type_name(elem), name));
                    } else {
                        let mut l = String::new();
                        let push = if class == SC_PUSH_CONSTANT { "[[vk::push_constant]] " } else { "" };
                        let _ = writeln!(l, "{}{}cbuffer {} {{", bind, push, name);
                        self.members(st, &mut l);
                        l.push('}');
                        groups[1].push(l);
                    }
                }
                SC_UNIFORM_CONSTANT => groups[2].push(format!("{}{};", bind, self.decl(ptr, &name))),
                SC_WORKGROUP => groups[3].push(format!("groupshared {};", self.decl(ptr, &name))),
                _ => {
                    let init = init.map_or(String::new(), |c| format!(" = {}", self.const_text(c).t));
                    let storage = if class == SC_PRIVATE { "static " } else { "" };
                    groups[3].push(format!("{}{}{};", storage, self.decl(ptr, &name), init));
                }
            }
        }
        for g in groups.iter_mut().filter(|g| !g.is_empty()) {
            g.sort();
            for l in g.iter() {
                out.push_str(l);
                out.push('\n');
            }
            out.push('\n');
        }
    }

    fn array_of_struct(&self, t: u32) -> Option<u32> {
        match self.ty(t) {
            Ty::Array(e, _) | Ty::RuntimeArray(e) => matches!(self.ty(e), Ty::Struct(_)).then_some(e),
            _ => None,
        }
    }

    // ------------------------------------------------------------------------------------------------------- function facts

    fn functions(&mut self) -> Vec<Fun> {
        let mut funs = vec![];
        let mut cur: Option<Fun> = None;
        let mut block = 0u32;
        let mut first = 0usize;
        let mut merge = Merge::None;
        let n = self.m.len();
        for k in 0..n {
            let i = self.m.ins(k);
            let o = i.o;
            match i.name {
                "OpFunction" if o.len() >= 2 => {
                    cur = Some(Fun { id: o[1], ret: o[0], ..Default::default() });
                }
                "OpFunctionEnd" => {
                    if let Some(f) = cur.take() {
                        funs.push(f);
                    }
                    continue;
                }
                _ => {}
            }
            let Some(f) = cur.as_mut() else { continue };
            match i.name {
                "OpFunctionParameter" if o.len() >= 2 => f.params.push(o[1]),
                "OpLabel" if !o.is_empty() => {
                    block = o[0];
                    first = k + 1;
                    merge = Merge::None;
                    if f.first_block == 0 {
                        f.first_block = block;
                    }
                }
                "OpSelectionMerge" if !o.is_empty() => merge = Merge::Sel(o[0]),
                "OpLoopMerge" if o.len() >= 2 => merge = Merge::Loop(o[0], o[1]),
                "OpVariable" => f.locals.push(k),
                "OpBranch" | "OpBranchConditional" | "OpSwitch" | "OpReturn" | "OpReturnValue" | "OpKill" | "OpUnreachable" | "OpTerminateInvocation" => {
                    f.blocks.insert(block, Block { first, term: k, merge });
                }
                "OpPhi" if o.len() >= 2 => {
                    let inc: Vec<(u32, u32)> = o[2..].chunks_exact(2).map(|c| (c[0], c[1])).collect();
                    f.phi_ids.insert(o[1]);
                    f.phis.entry(block).or_default().push((o[1], o[0], inc));
                }
                _ => {}
            }
            Self::count_uses(&i, f, block);
        }
        funs
    }

    /// Counts the id operands of `i`.  A use by a phi happens at the end of the predecessor, so it never folds: it clears `use_block`.
    fn count_uses(i: &Ins, f: &mut Fun, block: u32) {
        let Some(info) = i.info else { return };
        let phi = i.name == "OpPhi";
        let note = |id: u32, f: &mut Fun| {
            *f.uses.entry(id).or_insert(0) += 1;
            let b = f.use_block.entry(id).or_insert(block);
            if phi || *b != block {
                *b = 0;
            }
        };
        let o = i.o;
        let mut p = 0;
        for &(kind, quant) in info.operands {
            let mut first = true;
            while p < o.len() && (first || quant == 2) {
                first = false;
                match kind {
                    "IdRef" | "IdScope" | "IdMemorySemantics" => {
                        note(o[p], f);
                        p += 1;
                    }
                    "LiteralString" => p += string_at(&o[p..]).1,
                    "PairIdRefIdRef" => {
                        note(o[p], f);
                        p += 2;
                    }
                    "PairLiteralIntegerIdRef" | "PairIdRefLiteralInteger" => p += 2,
                    "ImageOperands" | "MemoryAccess" => {
                        // the words after the mask are ids (image operands) or literals (alignment); both are read by `rest_ids`
                        let mask = o[p];
                        p += 1;
                        if kind == "ImageOperands" {
                            for _ in 0..image_operand_words(mask) {
                                if p < o.len() {
                                    note(o[p], f);
                                    p += 1;
                                }
                            }
                        } else {
                            p = o.len();
                        }
                    }
                    "LiteralContextDependentNumber" => p = o.len(),
                    _ => p += 1,
                }
            }
        }
    }

    // ------------------------------------------------------------------------------------------------------- writing a function

    fn function(&mut self, f: &Fun, is_entry: bool) -> String {
        self.hoist.clear();
        self.labels.clear();
        for _ in 0..4 {
            self.reset_run(f);
            self.indent = 1;
            self.open = vec![0];
            self.next_scope = 1;
            self.region(f, f.first_block);
            let more = !self.want_hoist.is_subset(&self.hoist) || !self.want_labels.is_subset(&self.labels);
            self.hoist.extend(self.want_hoist.drain());
            self.labels.extend(self.want_labels.drain());
            if !more {
                break;
            }
        }
        let body = std::mem::take(&mut self.out);
        let mut s = String::new();
        let params: Vec<String> = f
            .params
            .iter()
            .map(|p| {
                let t = self.ty_of.get(p).copied().unwrap_or(0);
                let q = if matches!(self.ty(t), Ty::Pointer(..)) { "inout " } else { "" };
                format!("{}{}", q, self.decl(t, &self.name(*p)))
            })
            .collect();
        let fname = if is_entry { "main".to_string() } else { self.name(f.id) };
        let _ = writeln!(s, "{} {}({}) {{", self.type_name(f.ret), fname, params.join(", "));
        for &k in &f.locals {
            let i = self.m.ins(k);
            if i.o.len() < 3 || f.uses.get(&i.o[1]).copied().unwrap_or(0) == 0 {
                continue;
            }
            let init = i.o.get(3).map_or(String::new(), |c| format!(" = {}", self.const_text(*c).t));
            let _ = writeln!(s, "    {}{};", self.decl(i.o[0], &self.name(i.o[1])), init);
        }
        let mut phis: Vec<(u32, u32)> = f.phis.values().flatten().map(|p| (p.0, p.1)).collect();
        phis.sort_unstable();
        let mut hoisted: Vec<u32> = self.hoist.iter().copied().filter(|h| !f.phi_ids.contains(h)).collect();
        hoisted.sort_unstable();
        for (id, t) in phis {
            let _ = writeln!(s, "    {};", self.decl(t, &self.name(id)));
        }
        for id in hoisted {
            let t = self.ty_of.get(&id).copied().unwrap_or(0);
            let _ = writeln!(s, "    {};", self.decl(t, &self.name(id)));
        }
        s.push_str(&body);
        s.push_str("}\n");
        s
    }

    fn reset_run(&mut self, f: &Fun) {
        self.out.clear();
        self.vals.clear();
        self.pending.clear();
        self.sampled.clear();
        self.scope_of.clear();
        self.frames.clear();
        self.steps = 0;
        self.depth = 0;
        // repeated inlining of joins is the only thing that can grow: bounded, then a goto
        self.max_steps = f.blocks.len() * 6 + 256;
        for p in &f.params {
            let e = E::atom(self.name(*p));
            self.vals.insert(*p, e);
        }
        for &k in &f.locals {
            let i = self.m.ins(k);
            if i.o.len() >= 2 {
                let mut e = E::atom(self.name(i.o[1]));
                e.mem = true;
                self.vals.insert(i.o[1], e);
            }
        }
        for id in &f.phi_ids {
            let e = E { root: None, simple: false, ..E::atom(self.name(*id)) };
            self.vals.insert(*id, e);
        }
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn open_scope(&mut self, s: &str) {
        self.line(s);
        self.indent += 1;
        self.open.push(self.next_scope);
        self.next_scope += 1;
    }

    fn close_scope(&mut self, s: &str) {
        self.open.pop();
        self.indent = self.indent.saturating_sub(1);
        self.line(s);
    }

    fn label_here(&mut self, b: u32) {
        if self.labels.contains(&b) {
            let t = format!("_L{}:", b);
            self.line(&t);
        }
    }

    fn dest(&self, t: u32) -> Dest {
        for (k, fr) in self.frames.iter().rev().enumerate() {
            match fr {
                Frame::Sel { merge } => {
                    if *merge == t {
                        return if k == 0 { Dest::Fall } else { Dest::Goto };
                    }
                }
                Frame::Loop { header, merge, cont } => {
                    if *merge == t {
                        return Dest::Break;
                    }
                    if *header == t {
                        return Dest::Continue;
                    }
                    if *cont == t {
                        return Dest::ContBlock;
                    }
                    return if self.frames.iter().any(|f| Self::frame_targets(f, t)) { Dest::Goto } else { Dest::Inline };
                }
                Frame::Switch { merge } => {
                    if *merge == t {
                        return Dest::Break;
                    }
                    return if self.frames.iter().any(|f| Self::frame_targets(f, t)) { Dest::Goto } else { Dest::Inline };
                }
            }
        }
        Dest::Inline
    }

    fn frame_targets(f: &Frame, t: u32) -> bool {
        match f {
            Frame::Sel { merge } | Frame::Switch { merge } => *merge == t,
            Frame::Loop { header, merge, cont } => *merge == t || *header == t || *cont == t,
        }
    }

    /// The assignments of the phis of `to` on the edge from `from`.  Phis read each other's old value when a loop swaps them, so such
    /// a value is copied first.
    fn edge(&mut self, f: &Fun, from: u32, to: u32) {
        let Some(list) = f.phis.get(&to) else { return };
        let mut assigns = vec![];
        for (id, ty, inc) in list {
            if let Some((v, _)) = inc.iter().find(|x| x.1 == from) {
                assigns.push((*id, *ty, *v));
            }
        }
        let targets: HashSet<u32> = assigns.iter().map(|a| a.0).collect();
        let mut copies: HashMap<u32, String> = HashMap::new();
        for &(id, ty, v) in &assigns {
            if v != id && targets.contains(&v) && !copies.contains_key(&v) {
                let n = format!("{}_old", self.name(v));
                let d = self.decl(ty, &n);
                let src = self.name(v);
                self.line(&format!("{} = {};", d, src));
                copies.insert(v, n);
            }
        }
        for (id, _, v) in assigns {
            if v == id {
                continue;
            }
            let text = match copies.get(&v) {
                Some(c) => c.clone(),
                None => self.take(v).t,
            };
            let n = self.name(id);
            self.line(&format!("{} = {};", n, text));
        }
    }

    /// Writes the jump to `t` for a non-inline destination.
    fn jump(&mut self, f: &Fun, d: Dest, t: u32) {
        match d {
            Dest::Fall | Dest::Inline => {}
            Dest::Break => self.line("break;"),
            Dest::Continue => self.line("continue;"),
            Dest::ContBlock => self.region(f, t),
            Dest::Goto => {
                self.want_labels.insert(t);
                self.line(&format!("goto _L{};", t));
            }
        }
    }

    /// Writes the code from block `b` on, up to the end of the enclosing construct.
    fn region(&mut self, f: &Fun, b: u32) {
        self.depth += 1;
        let mut cur = b;
        loop {
            self.steps += 1;
            let Some(block) = f.blocks.get(&cur) else {
                self.warn(format!("branch to unknown block {}", cur));
                break;
            };
            if self.depth > MAX_DEPTH || self.steps > self.max_steps {
                self.warn("control flow too tangled: some joins are left as goto".to_string());
                self.want_labels.insert(cur);
                self.line(&format!("goto _L{};", cur));
                break;
            }
            self.cur = cur;
            if let Merge::Loop(merge, cont) = block.merge {
                if !self.frames.iter().any(|fr| matches!(fr, Frame::Loop { header, .. } if *header == cur)) {
                    self.open_scope("while (true) {");
                    self.frames.push(Frame::Loop { header: cur, merge, cont });
                    self.body_and_terminator(f, cur);
                    self.frames.pop();
                    if self.out.ends_with("continue;\n") {
                        let n = self.out.trim_end_matches("continue;\n").trim_end_matches(' ').len();
                        self.out.truncate(n);
                    }
                    self.close_scope("}");
                    self.label_here(merge);
                    match self.after_construct(f, merge) {
                        Some(next) => {
                            cur = next;
                            continue;
                        }
                        None => break,
                    }
                }
            }
            match self.step(f, cur) {
                Some(next) => cur = next,
                None => break,
            }
        }
        self.depth -= 1;
    }

    /// One pass through a loop header (the `while` is open already).
    fn body_and_terminator(&mut self, f: &Fun, b: u32) {
        self.depth += 1;
        if let Some(next) = self.step(f, b) {
            self.region(f, next);
        }
        self.depth -= 1;
    }

    /// Where to go after the `}` of a construct merging at `m`; None when the enclosing construct is finished.
    fn after_construct(&mut self, f: &Fun, m: u32) -> Option<u32> {
        let d = self.dest(m);
        match d {
            Dest::Inline => Some(m),
            Dest::Fall => None,
            _ => {
                self.jump(f, d, m);
                None
            }
        }
    }

    /// Writes block `b`; returns the block that follows at the same nesting, if any.
    fn step(&mut self, f: &Fun, b: u32) -> Option<u32> {
        let block = &f.blocks[&b];
        let (first, term, merge) = (block.first, block.term, block.merge);
        self.cur = b;
        for k in first..term {
            let i = self.m.ins(k);
            self.instr(f, &i);
        }
        let t = self.m.ins(term);
        let o = t.o;
        match t.name {
            "OpBranch" if !o.is_empty() => {
                let to = o[0];
                self.edge(f, b, to);
                let d = self.dest(to);
                if d == Dest::Inline {
                    return Some(to);
                }
                self.jump(f, d, to);
                None
            }
            "OpBranchConditional" if o.len() >= 3 => {
                let cond = self.take(o[0]);
                self.conditional(f, b, cond, o[1], o[2], merge)
            }
            "OpSwitch" if o.len() >= 2 => {
                let sel = self.take(o[0]);
                self.switch(f, b, sel, o, merge)
            }
            "OpReturn" => {
                self.line("return;");
                None
            }
            "OpReturnValue" if !o.is_empty() => {
                let v = self.take(o[0]);
                self.line(&format!("return {};", v.t));
                None
            }
            "OpKill" | "OpTerminateInvocation" => {
                self.line("discard;");
                None
            }
            _ => None,
        }
    }

    fn conditional(&mut self, f: &Fun, b: u32, cond: E, t: u32, e: u32, merge: Merge) -> Option<u32> {
        let has_phi = |to: u32| f.phis.get(&to).is_some_and(|l| l.iter().any(|p| p.2.iter().any(|x| x.1 == b)));
        if let Merge::Sel(m) = merge {
            self.frames.push(Frame::Sel { merge: m });
            let empty_then = t == m && !has_phi(t);
            let empty_else = e == m && !has_phi(e);
            if empty_then && empty_else {
                self.frames.pop();
            } else {
                let (first, second, c) = if empty_then { (e, None, negate(&cond)) } else { (t, (!empty_else).then_some(e), cond.t.clone()) };
                self.open_scope(&format!("if ({}) {{", c));
                self.arm(f, b, first);
                if let Some(s) = second {
                    self.close_scope("} else {");
                    self.open.push(self.next_scope);
                    self.next_scope += 1;
                    self.indent += 1;
                    self.arm(f, b, s);
                }
                self.close_scope("}");
                self.frames.pop();
            }
            self.label_here(m);
            return self.after_construct(f, m);
        }
        // no merge: a conditional break / continue, or the exit test of a loop.  When one side is a plain jump the other carries on at this level.
        let (dt, de) = (self.dest(t), self.dest(e));
        let plain = |d: Dest| matches!(d, Dest::Break | Dest::Continue | Dest::Goto | Dest::Fall);
        if plain(de) && dt == Dest::Inline {
            self.open_scope(&format!("if ({}) {{", negate(&cond)));
            self.edge(f, b, e);
            self.jump(f, de, e);
            self.close_scope("}");
            self.edge(f, b, t);
            return Some(t);
        }
        if plain(dt) && de == Dest::Inline {
            self.open_scope(&format!("if ({}) {{", cond.t));
            self.edge(f, b, t);
            self.jump(f, dt, t);
            self.close_scope("}");
            self.edge(f, b, e);
            return Some(e);
        }
        self.open_scope(&format!("if ({}) {{", cond.t));
        self.arm(f, b, t);
        self.close_scope("} else {");
        self.open.push(self.next_scope);
        self.next_scope += 1;
        self.indent += 1;
        self.arm(f, b, e);
        self.close_scope("}");
        None
    }

    fn arm(&mut self, f: &Fun, from: u32, to: u32) {
        self.edge(f, from, to);
        match self.dest(to) {
            Dest::Inline => self.region(f, to),
            d => self.jump(f, d, to),
        }
    }

    fn switch(&mut self, f: &Fun, b: u32, sel: E, o: &[u32], merge: Merge) -> Option<u32> {
        let default = o[1];
        let m = match merge {
            Merge::Sel(m) => m,
            _ => {
                self.warn("switch without a merge block".to_string());
                default
            }
        };
        // literals are 32 bits for the int selectors DXC emits; targets sharing a body share its labels
        let mut cases: Vec<(u32, Vec<String>)> = vec![];
        for c in o[2..].chunks_exact(2) {
            let lit = (c[0] as i32).to_string();
            match cases.iter_mut().find(|x| x.0 == c[1]) {
                Some(x) => x.1.push(lit),
                None => cases.push((c[1], vec![lit])),
            }
        }
        self.open_scope(&format!("switch ({}) {{", sel.t));
        self.frames.push(Frame::Switch { merge: m });
        for (target, labels) in &cases {
            let mut head: Vec<String> = labels.iter().map(|l| format!("case {}:", l)).collect();
            if *target == default {
                head.push("default:".to_string());
            }
            self.open_scope(&format!("{} {{", head.join(" ")));
            self.arm(f, b, *target);
            self.close_scope("}");
        }
        if !cases.iter().any(|c| c.0 == default) {
            self.open_scope("default: {");
            self.arm(f, b, default);
            self.close_scope("}");
        }
        self.frames.pop();
        self.close_scope("}");
        self.label_here(m);
        self.after_construct(f, m)
    }

    // ------------------------------------------------------------------------------------------------------- values

    /// The expression of `id` for one use: a pending fold is consumed, a local records that it was seen from this scope.
    fn take(&mut self, id: u32) -> E {
        if self.consts.contains_key(&id) {
            return self.const_text(id);
        }
        if let Some(k) = self.pending.iter().position(|x| *x == id) {
            self.pending.remove(k);
            return self.vals.remove(&id).unwrap_or_else(|| E::atom(self.name(id)));
        }
        if let Some(e) = self.vals.get(&id).cloned() {
            if let Some(r) = e.root {
                self.seen(r);
            }
            return e;
        }
        // a global, or a value of a block not written yet
        self.seen(id);
        let mut e = E::atom(self.name(id));
        if let Some(g) = self.globals.iter().find(|g| g.0 == id) {
            e.mem = !self.read_only(g.2, g.0);
        }
        e
    }

    fn seen(&mut self, id: u32) {
        if let Some(s) = self.scope_of.get(&id) {
            if !self.open.contains(s) {
                self.want_hoist.insert(id);
            }
        }
    }

    fn read_only(&self, class: u32, var: u32) -> bool {
        match class {
            SC_INPUT | SC_UNIFORM_CONSTANT | SC_PUSH_CONSTANT => true,
            SC_UNIFORM => {
                let st = self.pointee(self.ty_of.get(&var).copied().unwrap_or_else(|| self.global_ty(var)));
                !self.has_deco(st, D_BUFFER_BLOCK) || self.has_deco(var, D_NONWRITABLE)
            }
            SC_STORAGE_BUFFER => self.has_deco(var, D_NONWRITABLE),
            _ => false,
        }
    }

    fn global_ty(&self, var: u32) -> u32 {
        self.globals.iter().find(|g| g.0 == var).map_or(0, |g| g.1)
    }

    /// Gives `r` its value: folded into the single user, kept as an alias, or written as a local.
    fn define(&mut self, f: &Fun, r: u32, ty: u32, e: E) {
        let uses = f.uses.get(&r).copied().unwrap_or(0);
        if uses == 0 {
            return;
        }
        if matches!(self.ty(ty), Ty::Pointer(..)) || (e.simple && !e.mem) {
            self.vals.insert(r, e);
            return;
        }
        if uses == 1 && f.use_block.get(&r).copied().unwrap_or(0) == self.cur {
            self.vals.insert(r, e);
            self.pending.push(r);
            return;
        }
        self.materialize(r, ty, e);
    }

    fn materialize(&mut self, r: u32, ty: u32, e: E) {
        let n = self.name(r);
        if self.hoist.contains(&r) {
            self.line(&format!("{} = {};", n, e.t));
        } else {
            let d = self.decl(ty, &n);
            self.line(&format!("{} = {};", d, e.t));
        }
        self.scope_of.insert(r, *self.open.last().unwrap_or(&0));
        let mut a = E::atom(n);
        a.root = Some(r);
        self.vals.insert(r, a);
    }

    /// Before a statement with side effects: pending reads of writable memory are written out, in their original order.
    fn flush_mem(&mut self) {
        let ids: Vec<u32> = self.pending.iter().copied().filter(|p| self.vals.get(p).is_some_and(|e| e.mem)).collect();
        for id in ids {
            self.pending.retain(|x| *x != id);
            if let Some(e) = self.vals.remove(&id) {
                let ty = self.ty_of.get(&id).copied().unwrap_or(0);
                self.materialize(id, ty, e);
            }
        }
    }

    fn bin(&self, op: &str, p: u8, a: &E, b: &E) -> E {
        E::new(format!("{} {} {}", a.wrap(p), op, b.wrap(p + 1)), p, a.mem || b.mem)
    }

    fn call(&self, name: &str, args: &[E]) -> E {
        let t: Vec<&str> = args.iter().map(|a| a.t.as_str()).collect();
        E::new(format!("{}({})", name, t.join(", ")), ATOM, args.iter().any(|a| a.mem))
    }

    /// Scalar or vector element type of `ty`.
    fn elem(&self, ty: u32) -> Ty {
        match self.ty(ty) {
            Ty::Vector(e, _) => self.ty(e),
            t => t,
        }
    }

    /// `(uint)x` when an operand's declared signedness is not the one the instruction works in.
    fn signed_as(&self, id: u32, e: E, want_signed: bool) -> E {
        let t = self.ty_of.get(&id).copied().or_else(|| self.consts.get(&id).map(|c| c.0)).unwrap_or(0);
        match self.elem(t) {
            Ty::Int(_, s) if s != want_signed => {
                let n = self.type_name(t);
                let to = if want_signed { n.replace("uint", "int") } else { n.replacen("int", "uint", 1) };
                E::new(format!("{}({})", to, e.t), ATOM, e.mem)
            }
            _ => e,
        }
    }

    fn index_step(&self, cur_ty: u32, idx: &str, const_idx: Option<u32>, text: &mut String) -> u32 {
        match self.ty(cur_ty) {
            Ty::Struct(ms) => {
                let k = const_idx.unwrap_or(0);
                if !self.wrappers.contains(&cur_ty) {
                    let _ = write!(text, ".{}", self.member_name(cur_ty, k));
                }
                ms.get(k as usize).copied().unwrap_or(0)
            }
            Ty::Vector(e, _) => {
                match const_idx {
                    Some(k) if k < 4 => {
                        text.push('.');
                        text.push(['x', 'y', 'z', 'w'][k as usize]);
                    }
                    _ => {
                        let _ = write!(text, "[{}]", idx);
                    }
                }
                e
            }
            Ty::Matrix(c, _) => {
                let _ = write!(text, "[{}]", idx);
                c
            }
            Ty::Array(e, _) | Ty::RuntimeArray(e) => {
                let _ = write!(text, "[{}]", idx);
                e
            }
            _ => {
                let _ = write!(text, "[{}]", idx);
                0
            }
        }
    }

    fn const_u32(&self, id: u32) -> Option<u32> {
        match self.consts.get(&id) {
            Some((_, Cv::Bits(b))) => Some(*b as u32),
            Some((_, Cv::Null)) => Some(0),
            _ => None,
        }
    }

    fn shuffle(&mut self, f: &Fun, ty: u32, r: u32, o: &[u32]) {
        let (v1, v2) = (o[2], o[3]);
        let comps = &o[4..];
        let n1 = match self.ty(self.ty_of.get(&v1).copied().or_else(|| self.consts.get(&v1).map(|c| c.0)).unwrap_or(0)) {
            Ty::Vector(_, n) => n,
            _ => 1,
        };
        let a = self.take(v1);
        let b = if v1 == v2 { a.clone() } else { self.take(v2) };
        let swz = |c: &[u32], base: u32| -> String { c.iter().map(|x| ['x', 'y', 'z', 'w'][((x.wrapping_sub(base)) & 3) as usize]).collect() };
        let e = if comps.iter().all(|c| *c < n1) {
            if comps.len() as u32 == n1 && comps.iter().enumerate().all(|(k, c)| k as u32 == *c) {
                a
            } else {
                let mut x = E::new(format!("{}.{}", a.wrap(ATOM), swz(comps, 0)), ATOM, a.mem);
                x.simple = a.simple;
                x.root = a.root;
                x
            }
        } else if comps.iter().all(|c| *c >= n1 && *c != u32::MAX) {
            let mut x = E::new(format!("{}.{}", b.wrap(ATOM), swz(comps, n1)), ATOM, b.mem);
            x.simple = b.simple;
            x.root = b.root;
            x
        } else {
            let mut parts = vec![];
            let mut k = 0;
            while k < comps.len() {
                let from_a = comps[k] < n1;
                let mut j = k;
                while j < comps.len() && (comps[j] < n1) == from_a {
                    j += 1;
                }
                let (src, base) = if from_a { (&a, 0) } else { (&b, n1) };
                let whole = from_a && j - k == n1 as usize && comps[k..j].iter().enumerate().all(|(q, c)| q as u32 == *c);
                parts.push(if whole { src.t.clone() } else { format!("{}.{}", src.wrap(ATOM), swz(&comps[k..j], base)) });
                k = j;
            }
            E::new(format!("{}({})", self.type_name(ty), parts.join(", ")), ATOM, a.mem || b.mem)
        };
        self.define(f, r, ty, e);
    }

    fn extract(&mut self, f: &Fun, ty: u32, r: u32, o: &[u32]) {
        let base = self.take(o[2]);
        let mut t = self.ty_of.get(&o[2]).copied().or_else(|| self.consts.get(&o[2]).map(|c| c.0)).unwrap_or(0);
        let mut text = base.wrap(ATOM);
        for &k in &o[3..] {
            t = self.index_step(t, &k.to_string(), Some(k), &mut text);
        }
        let mut e = E::new(text, ATOM, base.mem);
        e.simple = base.simple;
        e.root = base.root;
        self.define(f, r, ty, e);
    }

    fn access_chain(&mut self, f: &Fun, ty: u32, r: u32, o: &[u32]) {
        let base = self.take(o[2]);
        let mut t = self.pointee(self.ty_of.get(&o[2]).copied().unwrap_or_else(|| self.global_ty(o[2])));
        let mut text = base.t.clone();
        let mut mem = base.mem;
        let mut simple = true;
        for &id in &o[3..] {
            let c = self.const_u32(id);
            let idx = self.take(id);
            mem |= idx.mem;
            if c.is_none() {
                simple = false;
            }
            t = self.index_step(t, &idx.t, c, &mut text);
        }
        let e = E { t: text, p: ATOM, mem, simple, root: base.root };
        self.define(f, r, ty, e);
    }

    /// (image, sampler) text of a sampled image operand.
    fn sampled_image(&mut self, id: u32) -> (String, String) {
        if let Some(p) = self.sampled.get(&id) {
            return p.clone();
        }
        (self.take(id).t, "/* sampler */".to_string())
    }

    fn image_sample(&mut self, f: &Fun, name: &str, ty: u32, r: u32, o: &[u32]) {
        let dref = name.contains("Dref");
        let explicit = name.contains("Explicit");
        let (img, smp) = self.sampled_image(o[2]);
        let coord = self.take(o[3]);
        let mut p = 4;
        let reference = if dref {
            p += 1;
            o.get(4).map(|x| self.take(*x))
        } else {
            None
        };
        let ops = self.image_operands(o, p);
        let mut mem = coord.mem;
        let mut args = vec![smp, coord.t];
        let method = if let Some(rf) = reference {
            args.push(rf.t);
            if explicit {
                match ops.lod.as_ref() {
                    Some(l) if l.t == "0.0" => "SampleCmpLevelZero",
                    Some(l) => {
                        args.push(l.t.clone());
                        "SampleCmpLevel"
                    }
                    None => "SampleCmpLevelZero",
                }
            } else {
                "SampleCmp"
            }
        } else if let Some(g) = &ops.grad {
            args.push(g.0.t.clone());
            args.push(g.1.t.clone());
            "SampleGrad"
        } else if explicit {
            args.push(ops.lod.as_ref().map_or("0.0".to_string(), |l| l.t.clone()));
            "SampleLevel"
        } else if let Some(b) = &ops.bias {
            args.push(b.t.clone());
            "SampleBias"
        } else {
            "Sample"
        };
        if let Some(of) = &ops.offset {
            args.push(of.t.clone());
        }
        if let Some(c) = &ops.min_lod {
            args.push(format!("/* min lod */ {}", c.t));
        }
        mem |= ops.mem;
        if name.contains("Proj") {
            self.warn(format!("{}: projective sampling written as plain sampling", name));
        }
        let e = E::new(format!("{}.{}({})", img, method, args.join(", ")), ATOM, mem);
        self.define(f, r, ty, e);
    }

    fn image_operands(&mut self, o: &[u32], at: usize) -> ImageOps {
        let mut r = ImageOps::default();
        let Some(&mask) = o.get(at) else { return r };
        let mut p = at + 1;
        let mut next = |g: &mut Self| {
            let v = o.get(p).copied();
            p += 1;
            v.map(|x| g.take(x))
        };
        if mask & 1 != 0 {
            r.bias = next(self);
        }
        if mask & 2 != 0 {
            r.lod = next(self);
        }
        if mask & 4 != 0 {
            if let (Some(a), Some(b)) = (next(self), next(self)) {
                r.grad = Some((a, b));
            }
        }
        if mask & 8 != 0 || mask & 0x10 != 0 || mask & 0x20 != 0 {
            for bit in [8, 0x10, 0x20] {
                if mask & bit != 0 {
                    r.offset = next(self);
                }
            }
        }
        if mask & 0x40 != 0 {
            r.sample = next(self);
        }
        if mask & 0x80 != 0 {
            r.min_lod = next(self);
        }
        r.mem = [&r.bias, &r.lod, &r.offset, &r.sample, &r.min_lod].iter().any(|x| x.as_ref().is_some_and(|e| e.mem));
        r
    }

    /// The fallback for anything without a spelling: visible in the text and in the warnings.
    fn unknown(&mut self, f: &Fun, i: &Ins) {
        self.warn(format!("no HLSL spelling for {}", i.name));
        let info = i.info;
        let has_result = info.is_some_and(|x| x.operands.first().is_some_and(|o| o.0 == "IdResultType")) && i.o.len() >= 2;
        let first = if has_result { 2 } else { 0 };
        let kinds: Vec<&str> = info.map_or(vec![], |x| x.operands.iter().map(|o| o.0).collect());
        let mut args = vec![];
        for (k, w) in i.o.iter().enumerate().skip(first) {
            let is_id = kinds.get(k).map_or(false, |x| *x == "IdRef");
            args.push(if is_id { self.take(*w) } else { E::atom(w.to_string()) });
        }
        self.flush_mem();
        let mut e = self.call(i.name, &args);
        e.mem = true;
        if has_result {
            let (ty, r) = (i.o[0], i.o[1]);
            if f.uses.get(&r).copied().unwrap_or(0) > 0 {
                self.materialize(r, ty, e);
            } else {
                self.line(&format!("{};", e.t));
            }
        } else {
            self.line(&format!("{};", e.t));
        }
    }

    fn atomic(&mut self, f: &Fun, name: &str, i: &Ins) {
        let o = i.o;
        let (ty, r) = (o[0], o[1]);
        let func = match name {
            "OpAtomicIAdd" => "InterlockedAdd",
            "OpAtomicISub" => "InterlockedSub",
            "OpAtomicAnd" => "InterlockedAnd",
            "OpAtomicOr" => "InterlockedOr",
            "OpAtomicXor" => "InterlockedXor",
            "OpAtomicUMin" | "OpAtomicSMin" => "InterlockedMin",
            "OpAtomicUMax" | "OpAtomicSMax" => "InterlockedMax",
            "OpAtomicExchange" => "InterlockedExchange",
            "OpAtomicCompareExchange" => "InterlockedCompareExchange",
            _ => return self.unknown(f, i),
        };
        let dest = self.take(o[2]);
        // pointer, scope, semantics[, unequal semantics], value[, comparator]
        let mut args = vec![dest];
        if name == "OpAtomicCompareExchange" && o.len() >= 8 {
            let cmp = self.take(o[7]);
            let v = self.take(o[6]);
            args.push(cmp);
            args.push(v);
        } else if let Some(v) = o.get(5) {
            let v = self.take(*v);
            args.push(v);
        }
        self.flush_mem();
        if f.uses.get(&r).copied().unwrap_or(0) > 0 {
            let n = self.name(r);
            if !self.hoist.contains(&r) {
                let d = self.decl(ty, &n);
                self.line(&format!("{};", d));
            }
            self.scope_of.insert(r, *self.open.last().unwrap_or(&0));
            args.push(E::atom(n.clone()));
            let mut a = E::atom(n);
            a.root = Some(r);
            self.vals.insert(r, a);
        }
        let e = self.call(func, &args);
        self.line(&format!("{};", e.t));
    }

    fn ext_inst(&mut self, f: &Fun, i: &Ins) {
        let o = i.o;
        if o.len() < 4 {
            return;
        }
        let (ty, r) = (o[0], o[1]);
        if !self.glsl.contains(&o[2]) {
            // NonSemantic sets carry no behaviour
            return;
        }
        let name = GLSL_STD_450.binary_search_by_key(&o[3], |g| g.0).map_or("?", |k| GLSL_STD_450[k].1);
        let args: Vec<E> = o[4..].iter().map(|a| self.take(*a)).collect();
        let func = match name {
            "Round" | "RoundEven" => "round",
            "Trunc" => "trunc",
            "FAbs" | "SAbs" => "abs",
            "FSign" | "SSign" => "sign",
            "Floor" => "floor",
            "Ceil" => "ceil",
            "Fract" => "frac",
            "Radians" => "radians",
            "Degrees" => "degrees",
            "Sin" => "sin",
            "Cos" => "cos",
            "Tan" => "tan",
            "Asin" => "asin",
            "Acos" => "acos",
            "Atan" => "atan",
            "Sinh" => "sinh",
            "Cosh" => "cosh",
            "Tanh" => "tanh",
            "Atan2" => "atan2",
            "Pow" => "pow",
            "Exp" => "exp",
            "Log" => "log",
            "Exp2" => "exp2",
            "Log2" => "log2",
            "Sqrt" => "sqrt",
            "InverseSqrt" => "rsqrt",
            "Determinant" => "determinant",
            "FMin" | "UMin" | "SMin" | "NMin" => "min",
            "FMax" | "UMax" | "SMax" | "NMax" => "max",
            "FClamp" | "UClamp" | "SClamp" | "NClamp" => "clamp",
            "FMix" => "lerp",
            "Step" => "step",
            "SmoothStep" => "smoothstep",
            "Fma" => "mad",
            "Ldexp" => "ldexp",
            "Length" => "length",
            "Distance" => "distance",
            "Cross" => "cross",
            "Normalize" => "normalize",
            "FaceForward" => "faceforward",
            "Reflect" => "reflect",
            "Refract" => "refract",
            "FindILsb" => "firstbitlow",
            "FindSMsb" | "FindUMsb" => "firstbithigh",
            other => {
                let g = format!("glsl_{}", other);
                let e = self.call(&g, &args);
                return self.define(f, r, ty, e);
            }
        };
        let e = self.call(func, &args);
        self.define(f, r, ty, e);
    }

    fn instr(&mut self, f: &Fun, i: &Ins) {
        let o = i.o;
        let name = i.name;
        match name {
            "OpNop" | "OpLine" | "OpNoLine" | "OpLabel" | "OpSelectionMerge" | "OpLoopMerge" | "OpPhi" | "OpVariable" | "OpFunction" | "OpFunctionParameter"
            | "OpUndef" | "OpName" | "OpMemberName" | "OpModuleProcessed" | "OpSource" | "OpString" => return,
            _ => {}
        }
        // statements without a result
        match name {
            "OpStore" if o.len() >= 2 => {
                let v = self.take(o[1]);
                let p = self.take(o[0]);
                self.flush_mem();
                return self.line(&format!("{} = {};", p.t, v.t));
            }
            "OpCopyMemory" if o.len() >= 2 => {
                let s = self.take(o[1]);
                let d = self.take(o[0]);
                self.flush_mem();
                return self.line(&format!("{} = {};", d.t, s.t));
            }
            "OpImageWrite" if o.len() >= 3 => {
                let v = self.take(o[2]);
                let c = self.take(o[1]);
                let img = self.take(o[0]);
                self.flush_mem();
                return self.line(&format!("{}[{}] = {};", img.t, c.t, v.t));
            }
            "OpAtomicStore" if o.len() >= 4 => {
                let v = self.take(o[3]);
                let p = self.take(o[0]);
                self.flush_mem();
                return self.line(&format!("InterlockedExchange({}, {}, /* discarded */);", p.t, v.t));
            }
            "OpControlBarrier" => {
                self.flush_mem();
                return self.line("GroupMemoryBarrierWithGroupSync();");
            }
            "OpMemoryBarrier" => {
                self.flush_mem();
                return self.line("AllMemoryBarrier();");
            }
            "OpDemoteToHelperInvocation" | "OpDemoteToHelperInvocationEXT" => {
                self.flush_mem();
                return self.line("discard;");
            }
            "OpEmitVertex" => {
                self.flush_mem();
                return self.line("stream.Append(); // OpEmitVertex");
            }
            "OpEndPrimitive" => {
                self.flush_mem();
                return self.line("stream.RestartStrip(); // OpEndPrimitive");
            }
            "OpEmitStreamVertex" | "OpEndStreamPrimitive" => return self.unknown(f, i),
            _ => {}
        }
        let has_result = i.info.is_some_and(|x| x.operands.first().is_some_and(|k| k.0 == "IdResultType"));
        if !has_result || o.len() < 2 {
            return self.unknown(f, i);
        }
        let (ty, r) = (o[0], o[1]);
        let a = |k: usize| o.get(2 + k).copied().unwrap_or(0);
        let n_args = o.len() - 2;

        if let Some((op, prec)) = binary(name) {
            if n_args >= 2 {
                let x = self.take(a(0));
                let y = self.take(a(1));
                let (x, y) = match signedness(name) {
                    Some(s) => (self.signed_as(a(0), x, s), self.signed_as(a(1), y, s)),
                    None => (x, y),
                };
                let e = self.bin(op, prec, &x, &y);
                return self.define(f, r, ty, e);
            }
        }
        if let Some((func, _)) = unary_call(name) {
            if n_args >= 1 {
                let x = self.take(a(0));
                let e = self.call(func, &[x]);
                return self.define(f, r, ty, e);
            }
        }
        match name {
            "OpLoad" if n_args >= 1 => {
                let mut e = self.take(a(0));
                e.p = ATOM;
                if !e.mem {
                    e.simple = e.simple && e.root.is_none();
                } else {
                    e.simple = false;
                }
                self.define(f, r, ty, e)
            }
            "OpCopyObject" | "OpCopyLogical" if n_args >= 1 => {
                let e = self.take(a(0));
                self.define(f, r, ty, e)
            }
            "OpAccessChain" | "OpInBoundsAccessChain" if n_args >= 1 => self.access_chain(f, ty, r, o),
            "OpVectorShuffle" if n_args >= 2 => self.shuffle(f, ty, r, o),
            "OpCompositeExtract" if n_args >= 1 => self.extract(f, ty, r, o),
            "OpCompositeConstruct" => {
                let splat = o.len() > 3 && o[2..].iter().all(|x| *x == o[2]) && matches!(self.ty(ty), Ty::Vector(..));
                let args: Vec<E> = o[2..].iter().map(|x| self.take(*x)).collect();
                let tn = self.type_name(ty);
                if splat {
                    let e = E::new(format!("({}){}", tn, args[0].wrap(UNARY)), UNARY, args[0].mem);
                    return self.define(f, r, ty, e);
                }
                let mut e = self.call(&tn, &args);
                if matches!(self.ty(ty), Ty::Array(..) | Ty::Struct(_)) {
                    e.t = format!("{{ {} }}", args.iter().map(|x| x.t.as_str()).collect::<Vec<_>>().join(", "));
                }
                self.define(f, r, ty, e)
            }
            "OpCompositeInsert" if n_args >= 2 => {
                let v = self.take(a(0));
                let base = self.take(a(1));
                let mut t = ty;
                let n = self.name(r);
                let mut target = n.clone();
                for &k in o.get(4..).unwrap_or(&[]) {
                    t = self.index_step(t, &k.to_string(), Some(k), &mut target);
                }
                self.materialize(r, ty, base);
                self.line(&format!("{} = {};", target, v.t));
            }
            "OpCopyArrayElement" => self.unknown(f, i),
            "OpFNegate" | "OpSNegate" if n_args >= 1 => {
                let x = self.take(a(0));
                let e = E::new(format!("-{}", x.wrap(UNARY + 1)), UNARY, x.mem);
                self.define(f, r, ty, e)
            }
            "OpNot" | "OpLogicalNot" if n_args >= 1 => {
                let x = self.take(a(0));
                let c = if name == "OpNot" { "~" } else { "!" };
                let e = E::new(format!("{}{}", c, x.wrap(UNARY)), UNARY, x.mem);
                self.define(f, r, ty, e)
            }
            "OpFUnordLessThan" | "OpFUnordLessThanEqual" | "OpFUnordGreaterThan" | "OpFUnordGreaterThanEqual" if n_args >= 2 => {
                // true on NaN, which is the negation of the opposite ordered test
                let op = match name {
                    "OpFUnordLessThan" => ">=",
                    "OpFUnordLessThanEqual" => ">",
                    "OpFUnordGreaterThan" => "<=",
                    _ => "<",
                };
                let x = self.take(a(0));
                let y = self.take(a(1));
                let inner = self.bin(op, CMP, &x, &y);
                let e = E::new(format!("!({})", inner.t), UNARY, inner.mem);
                self.define(f, r, ty, e)
            }
            "OpSelect" if n_args >= 3 => {
                let c = self.take(a(0));
                let x = self.take(a(1));
                let y = self.take(a(2));
                let e = E::new(format!("{} ? {} : {}", c.wrap(TERN + 1), x.wrap(TERN + 1), y.wrap(TERN)), TERN, c.mem || x.mem || y.mem);
                self.define(f, r, ty, e)
            }
            "OpConvertFToU" | "OpConvertFToS" | "OpConvertSToF" | "OpConvertUToF" | "OpUConvert" | "OpSConvert" | "OpFConvert" | "OpSatConvertSToU" | "OpSatConvertUToS"
                if n_args >= 1 =>
            {
                let x = self.take(a(0));
                let tn = self.type_name(ty);
                let e = self.call(&tn, &[x]);
                self.define(f, r, ty, e)
            }
            "OpBitcast" if n_args >= 1 => {
                let x = self.take(a(0));
                let func = match self.elem(ty) {
                    Ty::Float(16) => "asfloat16",
                    Ty::Float(64) => "asdouble",
                    Ty::Float(_) => "asfloat",
                    Ty::Int(_, true) => "asint",
                    _ => "asuint",
                };
                let e = self.call(func, &[x]);
                self.define(f, r, ty, e)
            }
            "OpVectorTimesMatrix" | "OpMatrixTimesVector" | "OpMatrixTimesMatrix" if n_args >= 2 => {
                // operands swapped back to the HLSL order, see the module comment
                let x = self.take(a(0));
                let y = self.take(a(1));
                let e = self.call("mul", &[y, x]);
                self.define(f, r, ty, e)
            }
            "OpDot" if n_args >= 2 => {
                let x = self.take(a(0));
                let y = self.take(a(1));
                let e = self.call("dot", &[x, y]);
                self.define(f, r, ty, e)
            }
            "OpOuterProduct" if n_args >= 2 => {
                let x = self.take(a(0));
                let y = self.take(a(1));
                let e = self.call("outer_product", &[x, y]);
                self.define(f, r, ty, e)
            }
            "OpFMod" | "OpSMod" if n_args >= 2 => {
                // the result takes the divisor's sign, unlike %
                let x = self.take(a(0));
                let y = self.take(a(1));
                let e = self.call("mod", &[x, y]);
                self.define(f, r, ty, e)
            }
            "OpBitFieldInsert" | "OpBitFieldSExtract" | "OpBitFieldUExtract" => {
                let args: Vec<E> = o[2..].iter().map(|x| self.take(*x)).collect();
                let e = self.call(&name[2..], &args);
                self.define(f, r, ty, e)
            }
            "OpExtInst" => self.ext_inst(f, i),
            "OpSampledImage" if n_args >= 2 => {
                let img = self.take(a(0));
                let smp = self.take(a(1));
                self.sampled.insert(r, (img.t, smp.t));
            }
            "OpImage" if n_args >= 1 => {
                let (img, _) = self.sampled_image(a(0));
                self.vals.insert(r, E::atom(img));
            }
            "OpImageSampleImplicitLod" | "OpImageSampleExplicitLod" | "OpImageSampleDrefImplicitLod" | "OpImageSampleDrefExplicitLod" | "OpImageSampleProjImplicitLod"
            | "OpImageSampleProjExplicitLod" | "OpImageSampleProjDrefImplicitLod" | "OpImageSampleProjDrefExplicitLod"
                if n_args >= 2 =>
            {
                self.image_sample(f, name, ty, r, o)
            }
            "OpImageFetch" | "OpImageRead" if n_args >= 2 => {
                let img = self.take(a(0));
                let c = self.take(a(1));
                let ops = self.image_operands(o, 4);
                let mut args = vec![c.t.clone()];
                if let Some(l) = ops.lod.as_ref().or(ops.sample.as_ref()) {
                    args.push(l.t.clone());
                }
                if let Some(of) = &ops.offset {
                    args.push(of.t.clone());
                }
                let writable = name == "OpImageRead";
                let t = if writable && args.len() == 1 { format!("{}[{}]", img.t, c.t) } else { format!("{}.Load({})", img.t, args.join(", ")) };
                let e = E::new(t, ATOM, writable || c.mem || ops.mem);
                self.define(f, r, ty, e)
            }
            "OpImageGather" | "OpImageDrefGather" if n_args >= 3 => {
                let (img, smp) = self.sampled_image(a(0));
                let c = self.take(a(1));
                let third = self.take(a(2));
                let ops = self.image_operands(o, 5);
                let mut args = vec![smp, c.t.clone()];
                let method = if name == "OpImageDrefGather" {
                    args.push(third.t.clone());
                    "GatherCmp".to_string()
                } else {
                    format!("Gather{}", ["Red", "Green", "Blue", "Alpha"][(self.const_u32(a(2)).unwrap_or(0) & 3) as usize])
                };
                if let Some(of) = &ops.offset {
                    args.push(of.t.clone());
                }
                let e = E::new(format!("{}.{}({})", img, method, args.join(", ")), ATOM, c.mem || ops.mem);
                self.define(f, r, ty, e)
            }
            "OpImageQuerySizeLod" | "OpImageQuerySize" | "OpImageQueryLevels" | "OpImageQuerySamples" if n_args >= 1 => {
                let args: Vec<E> = o[2..].iter().map(|x| self.take(*x)).collect();
                let tag = &name["OpImageQuery".len()..];
                let e = self.call(&format!("GetDimensions_{}", tag), &args);
                self.define(f, r, ty, e)
            }
            "OpImageQueryLod" if n_args >= 2 => {
                let (img, smp) = self.sampled_image(a(0));
                let c = self.take(a(1));
                let e = E::new(format!("{}.CalculateLevelOfDetail({}, {})", img, smp, c.t), ATOM, c.mem);
                self.define(f, r, ty, e)
            }
            "OpImageTexelPointer" if n_args >= 3 => {
                let img = self.take(a(0));
                let c = self.take(a(1));
                let _ = self.take(a(2));
                self.vals.insert(r, E { t: format!("{}[{}]", img.t, c.t), p: ATOM, mem: true, simple: false, root: None });
            }
            "OpArrayLength" if n_args >= 1 => {
                let x = self.take(a(0));
                let e = self.call("array_length", &[x]);
                self.define(f, r, ty, e)
            }
            "OpFunctionCall" if n_args >= 1 => {
                let fname = self.name(a(0));
                let args: Vec<E> = o[3..].iter().map(|x| self.take(*x)).collect();
                self.flush_mem();
                let mut e = self.call(&fname, &args);
                e.mem = true;
                if f.uses.get(&r).copied().unwrap_or(0) > 0 {
                    self.materialize(r, ty, e);
                } else {
                    self.line(&format!("{};", e.t));
                }
            }
            "OpAtomicLoad" if n_args >= 1 => {
                let mut e = self.take(a(0));
                e.mem = true;
                self.define(f, r, ty, e)
            }
            "OpGroupNonUniformElect" => {
                let mut e = self.call("WaveIsFirstLane", &[]);
                e.mem = true;
                self.define(f, r, ty, e)
            }
            "OpGroupNonUniformAll" | "OpGroupNonUniformAny" | "OpGroupNonUniformAllEqual" | "OpGroupNonUniformBallot" | "OpGroupNonUniformBroadcastFirst" if n_args >= 2 => {
                let func = match name {
                    "OpGroupNonUniformAll" => "WaveActiveAllTrue",
                    "OpGroupNonUniformAny" => "WaveActiveAnyTrue",
                    "OpGroupNonUniformAllEqual" => "WaveActiveAllEqual",
                    "OpGroupNonUniformBallot" => "WaveActiveBallot",
                    _ => "WaveReadLaneFirst",
                };
                let x = self.take(a(1));
                let mut e = self.call(func, &[x]);
                e.mem = true;
                self.define(f, r, ty, e)
            }
            _ if n_args >= 3 && wave_reduce(name).is_some() => {
                // scope, group operation, value.  Marked as reading memory so a fold cannot carry it past a statement: the result
                // depends on which lanes are active, unlike a pure expression
                let (active, prefix, inclusive) = wave_reduce(name).unwrap_or(("", "", ""));
                let func = match a(1) {
                    0 => active,
                    1 => inclusive,
                    2 => prefix,
                    _ => return self.unknown(f, i),
                };
                let x = self.take(a(2));
                let mut e = self.call(func, &[x]);
                e.mem = true;
                self.define(f, r, ty, e)
            }
            _ if name.starts_with("OpAtomic") && n_args >= 3 => self.atomic(f, name, i),
            _ => self.unknown(f, i),
        }
    }
}

#[derive(Default)]
struct ImageOps {
    bias: Option<E>,
    lod: Option<E>,
    grad: Option<(E, E)>,
    offset: Option<E>,
    sample: Option<E>,
    min_lod: Option<E>,
    mem: bool,
}

fn negate(c: &E) -> String {
    if let Some(s) = c.t.strip_prefix('!') {
        if c.p == UNARY && !s.starts_with('(') {
            return s.to_string();
        }
    }
    format!("!{}", c.wrap(UNARY))
}

/// Words taken by the operands of an image operand mask (Grad takes two).
fn image_operand_words(mask: u32) -> u32 {
    (mask & 0xff).count_ones() + ((mask & 4 != 0) as u32)
}

fn binary(name: &str) -> Option<(&'static str, u8)> {
    Some(match name {
        "OpFAdd" | "OpIAdd" => ("+", ADD),
        "OpFSub" | "OpISub" => ("-", ADD),
        "OpFMul" | "OpIMul" | "OpVectorTimesScalar" | "OpMatrixTimesScalar" => ("*", MUL),
        "OpFDiv" | "OpSDiv" | "OpUDiv" => ("/", MUL),
        "OpUMod" | "OpSRem" | "OpFRem" => ("%", MUL),
        "OpShiftLeftLogical" => ("<<", SHIFT),
        "OpShiftRightLogical" | "OpShiftRightArithmetic" => (">>", SHIFT),
        "OpBitwiseAnd" => ("&", BAND),
        "OpBitwiseXor" => ("^", BXOR),
        "OpBitwiseOr" => ("|", BOR),
        "OpLogicalAnd" => ("&&", LAND),
        "OpLogicalOr" => ("||", LOR),
        "OpLogicalEqual" | "OpIEqual" | "OpFOrdEqual" | "OpFUnordEqual" => ("==", EQ),
        "OpLogicalNotEqual" | "OpINotEqual" | "OpFOrdNotEqual" | "OpFUnordNotEqual" => ("!=", EQ),
        "OpUGreaterThan" | "OpSGreaterThan" | "OpFOrdGreaterThan" => (">", CMP),
        "OpUGreaterThanEqual" | "OpSGreaterThanEqual" | "OpFOrdGreaterThanEqual" => (">=", CMP),
        "OpULessThan" | "OpSLessThan" | "OpFOrdLessThan" => ("<", CMP),
        "OpULessThanEqual" | "OpSLessThanEqual" | "OpFOrdLessThanEqual" => ("<=", CMP),
        _ => return None,
    })
}

/// Whether an integer instruction fixes the signedness of its operands (true = signed).
fn signedness(name: &str) -> Option<bool> {
    match name {
        "OpSDiv" | "OpSRem" | "OpSGreaterThan" | "OpSGreaterThanEqual" | "OpSLessThan" | "OpSLessThanEqual" | "OpShiftRightArithmetic" => Some(true),
        "OpUDiv" | "OpUMod" | "OpUGreaterThan" | "OpUGreaterThanEqual" | "OpULessThan" | "OpULessThanEqual" | "OpShiftRightLogical" => Some(false),
        _ => None,
    }
}

/// HLSL spellings of a group operation: (Reduce, ExclusiveScan, InclusiveScan).  HLSL has no inclusive scan intrinsic and no reducing
/// form of the ballot count on a given mask, so those names are descriptive only.
fn wave_reduce(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match name {
        "OpGroupNonUniformFAdd" | "OpGroupNonUniformIAdd" => ("WaveActiveSum", "WavePrefixSum", "WaveInclusiveSum"),
        "OpGroupNonUniformFMul" | "OpGroupNonUniformIMul" => ("WaveActiveProduct", "WavePrefixProduct", "WaveInclusiveProduct"),
        "OpGroupNonUniformFMin" | "OpGroupNonUniformSMin" | "OpGroupNonUniformUMin" => ("WaveActiveMin", "WavePrefixMin", "WaveInclusiveMin"),
        "OpGroupNonUniformFMax" | "OpGroupNonUniformSMax" | "OpGroupNonUniformUMax" => ("WaveActiveMax", "WavePrefixMax", "WaveInclusiveMax"),
        "OpGroupNonUniformBallotBitCount" => ("BallotCountBits", "BallotPrefixCountBits", "BallotInclusiveCountBits"),
        _ => return None,
    })
}

fn unary_call(name: &str) -> Option<(&'static str, ())> {
    Some((
        match name {
            "OpAny" => "any",
            "OpAll" => "all",
            "OpIsNan" => "isnan",
            "OpIsInf" => "isinf",
            "OpTranspose" => "transpose",
            "OpDPdx" => "ddx",
            "OpDPdy" => "ddy",
            "OpFwidth" => "fwidth",
            "OpDPdxFine" => "ddx_fine",
            "OpDPdyFine" => "ddy_fine",
            "OpDPdxCoarse" => "ddx_coarse",
            "OpDPdyCoarse" => "ddy_coarse",
            "OpFwidthFine" => "fwidth",
            "OpFwidthCoarse" => "fwidth",
            "OpBitCount" => "countbits",
            "OpBitReverse" => "reversebits",
            "OpQuantizeToF16" => "quantize_to_f16",
            _ => return None,
        },
        (),
    ))
}
