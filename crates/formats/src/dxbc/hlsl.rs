//! HLSL-like decompiler for decoded SM4/SM5 programs.
//!
//! The output is a readable rendering of the byte code, not the original source (INFERRED). Typing: temporaries are untyped
//! 32-bit registers in the byte code; each `rN` is declared with the type most of its writes produce and every access of
//! another type is wrapped in `asfloat/asint/asuint` (bit casts), so the semantics are preserved. Single-use temporaries are
//! folded into their reader only when provably safe (see `fold_list`).
//!
//! Pseudo-functions used where HLSL has no spelling (named like the instruction): `imul_hi`, `umul_hi`, `ubfe`, `ibfe`, `bfi`,
//! `uaddc_carry`, `usubb_borrow`, `LoadStructured`, `StoreStructured`, `resinfo`, `bufinfo`, `sampleinfo`, `samplepos`, `Sync_*`.

use super::disasm::{collect_res, dim_name, instr_text, ndst, sysval_name, ResInfo};
use super::expr::*;
use super::ops::*;
use super::reflect::*;
use super::Program;
use std::collections::{BTreeMap, BTreeSet, HashMap};

fn vtype_name(v: u16) -> &'static str {
    match v {
        1 => "bool",
        2 => "int",
        3 => "float",
        19 => "uint",
        20 => "uint8_t",
        39 => "double",
        57 => "min8float",
        58 => "min10float",
        59 => "min16float",
        60 => "min12int",
        61 => "min16int",
        62 => "min16uint",
        _ => "float",
    }
}

/// HLSL spelling of a reflection type (arrays are not included).
pub fn type_decl(t: &RType) -> String {
    match t.class {
        0 => vtype_name(t.vtype).to_string(),
        1 => format!("{}{}", vtype_name(t.vtype), t.cols),
        2 => format!("row_major {}{}x{}", vtype_name(t.vtype), t.rows, t.cols),
        3 => format!("{}{}x{}", vtype_name(t.vtype), t.rows, t.cols),
        5 => {
            if t.name.is_empty() {
                "struct_anon".into()
            } else {
                t.name.clone()
            }
        }
        _ => {
            if t.name.is_empty() {
                "object".into()
            } else {
                t.name.clone()
            }
        }
    }
}

fn vtype_ty(v: u16) -> Ty {
    match v {
        2 => Ty::I,
        1 | 19 | 20 => Ty::U,
        _ => Ty::F,
    }
}

enum Stmt {
    Assign { reg: u32, mask: u8, e: Ex, pure: bool, rid1: usize },
    Eff(Ex),
    If { cond: Ex, nz: bool, then: Vec<Stmt>, els: Vec<Stmt> },
    Loop(Vec<Stmt>),
    Switch { sel: Ex, body: Vec<Stmt> },
    Label(String),
    Jump { kind: &'static str, cond: Option<(Ex, bool)> },
    Comment(String),
}

fn comps_of(mask: u8) -> Vec<u8> {
    let v: Vec<u8> = (0..4u8).filter(|c| mask & (1 << c) != 0).collect();
    if v.is_empty() {
        vec![0]
    } else {
        v
    }
}

fn letters(c: &[u8]) -> String {
    c.iter().map(|&x| ['x', 'y', 'z', 'w'][x as usize & 3]).collect()
}

struct TexInfo {
    dim: u32,
    ret: Ty,
    ncoord: usize,
    ngrad: usize,
    ms: bool,
}

fn ret_ty(word: u32) -> Ty {
    match word & 15 {
        3 => Ty::I,
        4 => Ty::U,
        _ => Ty::F,
    }
}

/// Coordinate components (including the array slice) and gradient components for a D3D10_SB_RESOURCE_DIMENSION.
fn dim_coords(d: u32) -> (usize, usize) {
    match d {
        1 => (1, 1),
        2 => (1, 1),
        3 | 4 => (2, 2),
        5 => (3, 3),
        6 => (3, 3),
        7 => (2, 1),
        8 | 9 => (3, 2),
        10 => (4, 3),
        11 => (1, 1),
        12 => (2, 1),
        _ => (4, 3),
    }
}

fn tex_type_name(d: u32) -> &'static str {
    match d {
        1 => "Buffer",
        2 => "Texture1D",
        3 => "Texture2D",
        4 => "Texture2DMS",
        5 => "Texture3D",
        6 => "TextureCube",
        7 => "Texture1DArray",
        8 => "Texture2DArray",
        9 => "Texture2DMSArray",
        10 => "TextureCubeArray",
        11 => "ByteAddressBuffer",
        12 => "StructuredBuffer",
        _ => "Texture2D",
    }
}

fn srv_dim_to_sb(d: u32) -> u32 {
    match d {
        1 | 11 => 1,
        2 => 2,
        3 => 7,
        4 => 3,
        5 => 8,
        6 => 4,
        7 => 9,
        8 => 5,
        9 => 6,
        10 => 10,
        _ => 3,
    }
}

#[derive(Clone)]
struct Field {
    name: String,
    lo: u8,
    width: u8,
    ty: Ty,
}

fn field_name(e: &SigElem) -> String {
    if e.name.starts_with("SV_") && e.index == 0 {
        e.name.clone()
    } else {
        format!("{}{}", e.name, e.index)
    }
}

fn sig_field(e: &SigElem) -> Field {
    let lo = e.mask.trailing_zeros().min(3) as u8;
    let hi = (7 - e.mask.leading_zeros().min(7)) as u8;
    let ty = match e.ctype {
        1 => Ty::U,
        2 => Ty::I,
        _ => Ty::F,
    };
    Field { name: field_name(e), lo, width: hi.saturating_sub(lo) + 1, ty }
}

fn find_field(sig: &[SigElem], reg: u32, comp: u8) -> Option<Field> {
    sig.iter().find(|e| e.reg == reg && e.mask & (1 << comp) != 0).map(sig_field)
}

/// One resolved component before grouping.
struct CompRef {
    base: String,
    comp: String,
    /// Width of the base object (1 = scalar).
    bw: u8,
    ty: Ty,
}

fn compose(refs: Vec<CompRef>, want: Ty) -> Ex {
    // group consecutive components of the same base
    let mut groups: Vec<(String, Vec<String>, u8, Ty)> = Vec::new();
    for r in refs {
        match groups.last_mut() {
            Some(g) if g.0 == r.base => g.1.push(r.comp),
            _ => groups.push((r.base, vec![r.comp], r.bw, r.ty)),
        }
    }
    let mut exs: Vec<Ex> = Vec::new();
    for (base, comps, bw, ty) in groups {
        let n = comps.len() as u8;
        let text = if bw == 1 && comps.iter().all(|c| c.is_empty()) {
            if n == 1 {
                base
            } else {
                format!("{}.{}", base, "x".repeat(n as usize))
            }
        } else if comps.iter().all(|c| c.starts_with('_')) {
            format!("{}.{}", base, comps.concat())
        } else if bw == n && comps.iter().enumerate().all(|(i, c)| c == &["x", "y", "z", "w"][i]) {
            base
        } else {
            format!("{}.{}", base, comps.concat())
        };
        exs.push(coerce(Ex::raw(text, ty, n), want));
    }
    if exs.len() == 1 {
        return exs.pop().unwrap();
    }
    let ty = if want == Ty::Any { exs[0].ty } else { want };
    let w = exs.iter().map(|e| e.w).sum();
    let items = exs.into_iter().map(|e| coerce(e, ty)).collect();
    Ex::new(Node::Vecn(items), ty, w)
}

struct Tr<'a> {
    p: &'a Program,
    res: ResInfo,
    uavs: HashMap<u32, (u32, u32)>,
    uav_kind: HashMap<u32, u8>,
    tgsm_structured: BTreeSet<u32>,
    smp_cmp: BTreeSet<u32>,
    reads: Vec<ReadInfo>,
    regty: Vec<Ty>,
    votes: Vec<[u32; 3]>,
    impure: bool,
    phase: u8,
    raw_cbs: BTreeSet<u32>,
    cb_used: BTreeSet<(u32, String)>,
    builtins: BTreeMap<String, String>,
    ps_interp: HashMap<u32, u32>,
    warnings: Vec<String>,
    warned: BTreeSet<String>,
    uses_icb: bool,
    gs_stream: bool,
}

fn is_ps(p: &Program) -> bool {
    p.stage == 0
}

impl<'a> Tr<'a> {
    fn new(p: &'a Program, regty: Vec<Ty>) -> Self {
        let mut uavs = HashMap::new();
        let mut uav_kind = HashMap::new();
        let mut tgsm_structured = BTreeSet::new();
        let mut smp_cmp = BTreeSet::new();
        let mut ps_interp = HashMap::new();
        for i in &p.instrs {
            let n = i.ops.first().and_then(|o| o.index(0));
            match (i.op, n) {
                (156, Some(n)) => {
                    uavs.insert(n, (i.control & 31, i.extra.first().copied().unwrap_or(0)));
                    uav_kind.insert(n, 0u8);
                }
                (157, Some(n)) => {
                    uav_kind.insert(n, 1);
                }
                (158, Some(n)) => {
                    uav_kind.insert(n, 2);
                }
                (160, Some(n)) => {
                    tgsm_structured.insert(n);
                }
                (90, Some(n)) if i.control & 15 == 1 => {
                    smp_cmp.insert(n);
                }
                (98 | 99 | 100, Some(n)) => {
                    ps_interp.insert(n, i.control & 15);
                }
                _ => {}
            }
        }
        Tr {
            p,
            res: collect_res(&p.instrs),
            uavs,
            uav_kind,
            tgsm_structured,
            smp_cmp,
            reads: Vec::new(),
            regty,
            votes: Vec::new(),
            impure: false,
            phase: 0,
            raw_cbs: BTreeSet::new(),
            cb_used: BTreeSet::new(),
            builtins: BTreeMap::new(),
            ps_interp,
            warnings: Vec::new(),
            warned: BTreeSet::new(),
            uses_icb: false,
            gs_stream: false,
        }
    }

    fn warn(&mut self, key: &str, msg: String) {
        if self.warned.insert(key.to_string()) {
            self.warnings.push(msg);
        }
    }

    fn rty(&self, reg: u32) -> Ty {
        self.regty.get(reg as usize).copied().unwrap_or(Ty::F)
    }

    // ---------------------------------------------------------------- names

    fn rdef_res(&self, classes: &[u32], idx: u32) -> Option<String> {
        let r = self.p.rdef.as_ref()?;
        let b = r.resources.iter().find(|b| classes.contains(&b.kind) && idx >= b.bind_point && idx < b.bind_point + b.bind_count.max(1))?;
        if b.bind_count > 1 {
            Some(format!("{}[{}]", b.name, idx - b.bind_point))
        } else {
            Some(b.name.clone())
        }
    }
    fn tex_name(&self, i: u32) -> String {
        self.rdef_res(&[1, 2, 5, 7], i).unwrap_or_else(|| format!("t{}", i))
    }
    fn smp_name(&self, i: u32) -> String {
        self.rdef_res(&[3], i).unwrap_or_else(|| format!("s{}", i))
    }
    fn uav_name(&self, i: u32) -> String {
        self.rdef_res(&[4, 6, 8, 9, 10, 11], i).unwrap_or_else(|| format!("u{}", i))
    }
    fn cb_for_slot(&self, slot: u32) -> Option<&'a CBuffer> {
        let r = self.p.rdef.as_ref()?;
        let b = r.resources.iter().find(|b| b.kind == 0 && b.bind_point == slot)?;
        r.cbuffers.iter().find(|c| c.name == b.name)
    }

    fn tex_info(&mut self, t: u32) -> TexInfo {
        let (dim, ret, ms) = match self.res.dims.get(&t) {
            Some((d, rt, _)) => (*d, ret_ty(*rt), false),
            None => match self.p.rdef.as_ref().and_then(|r| r.resources.iter().find(|b| (b.kind == 2 || b.kind == 1) && b.bind_point == t)) {
                Some(b) => (srv_dim_to_sb(b.dim), if b.ret == 3 { Ty::I } else if b.ret == 4 { Ty::U } else { Ty::F }, false),
                None => {
                    self.warn("texinfo", format!("no declaration for texture t{}; assuming Texture2D", t));
                    (3, Ty::F, false)
                }
            },
        };
        let (nc, ng) = dim_coords(dim);
        TexInfo { dim, ret, ncoord: nc, ngrad: ng, ms: ms || dim == 4 || dim == 9 }
    }

    // ---------------------------------------------------------------- operand reading

    fn idx_ex(&mut self, i: &Idx) -> Ex {
        match i {
            Idx::Imm(v) => lit(vec![*v], Ty::I),
            Idx::Rel(v, r) => {
                let mut reg = self.read(r, &[0], Ty::Any);
                if reg.ty == Ty::F {
                    reg = coerce(reg, Ty::U);
                }
                if *v == 0 {
                    reg
                } else {
                    let t = if reg.ty == Ty::Any { Ty::U } else { reg.ty };
                    Ex::bin("+", 60, reg, lit(vec![*v], t), t)
                }
            }
        }
    }

    fn input_prefix(&mut self, o: &Operand, sig_is_in: bool) -> (String, Option<Ex>) {
        // 2-D operands (geometry/hull/domain control points) index the vertex first
        let base = if sig_is_in { "i" } else { "o" };
        if o.idx.len() >= 2 {
            let ix = self.idx_ex(&o.idx[0].clone());
            (base.to_string(), Some(ix))
        } else {
            (base.to_string(), None)
        }
    }

    fn sig_read(&mut self, sig_kind: u8, reg: u32, comps: &[u8], vtx: Option<Ex>, prefix: &str, want: Ty) -> Ex {
        let sig: &[SigElem] = match sig_kind {
            0 => &self.p.isg,
            1 => &self.p.osg,
            _ => &self.p.psg,
        };
        let mut refs = Vec::new();
        for &c in comps {
            match find_field(sig, reg, c) {
                Some(f) => {
                    let base = format!("{}.{}", prefix, f.name);
                    refs.push(CompRef { base, comp: ["x", "y", "z", "w"][(c - f.lo.min(c)) as usize & 3].to_string(), bw: f.width, ty: f.ty });
                }
                None => refs.push(CompRef { base: format!("{}.v{}", prefix, reg), comp: ["x", "y", "z", "w"][c as usize & 3].to_string(), bw: 4, ty: Ty::F }),
            }
        }
        if let Some(v) = vtx {
            // `i[vertex].FIELD`: build via Fmt, replacing the first '.' after the prefix
            let ex = compose(refs, want);
            if let Node::Raw(s) = &ex.n {
                let (a, b) = s.split_once('.').unwrap_or((s.as_str(), ""));
                return Ex::new(Node::Fmt(vec![Piece::S(format!("{}[", a)), Piece::E(v), Piece::S(format!("].{}", b))]), ex.ty, ex.w);
            }
            return ex;
        }
        compose(refs, want)
    }

    fn cb_read(&mut self, o: &Operand, comps: &[u8], want: Ty) -> Ex {
        let slot = o.index(0).unwrap_or(0);
        let last = o.idx.last().cloned().unwrap_or(Idx::Imm(0));
        let cb = self.cb_for_slot(slot);
        match (&last, cb) {
            (Idx::Imm(reg), Some(cb)) if o.idx.len() <= 2 => {
                let mut refs = Vec::new();
                for &c in comps {
                    match cb.resolve(*reg, c as u32) {
                        Some(r) => {
                            let root = r.base.split(|c| c == '.' || c == '[').next().unwrap_or("").to_string();
                            self.cb_used.insert((slot, root));
                            refs.push(CompRef { bw: r.width, base: r.base, comp: r.comp, ty: vtype_ty(r.vtype) })
                        }
                        None => {
                            self.raw_cbs.insert(slot);
                            refs.push(CompRef { base: format!("cb{}[{}]", slot, reg), comp: ["x", "y", "z", "w"][c as usize & 3].to_string(), bw: 4, ty: Ty::F });
                        }
                    }
                }
                compose(refs, want)
            }
            _ => {
                // relative addressing or no reflection
                let ix = self.idx_ex(&last);
                let sw = letters(comps);
                // array of 16-byte elements in reflection: g_Array[idx + k]
                if let (Some(cb), Idx::Rel(imm, _)) = (cb, &last) {
                    if let Some(v) = cb.vars.iter().find(|v| v.ty.elements > 1 && v.ty.class <= 1 && v.offset % 16 == 0 && *imm * 16 >= v.offset && *imm * 16 < v.offset + type_size(&v.ty).max(1)) {
                        let first = v.offset / 16;
                        let idx_only = if let Idx::Rel(_, r) = &last {
                            let x = self.read(r, &[0], Ty::Any);
                            if x.ty == Ty::F { coerce(x, Ty::U) } else { x }
                        } else {
                            ix.clone()
                        };
                        let rel = *imm - first;
                        let it = if idx_only.ty == Ty::Any { Ty::U } else { idx_only.ty };
                        let idxe = if rel == 0 { idx_only } else { Ex::bin("+", 60, idx_only, lit(vec![rel], it), it) };
                        let ty = vtype_ty(v.ty.vtype);
                        let ex = Ex::new(Node::Fmt(vec![Piece::S(format!("{}[", v.name)), Piece::E(idxe), Piece::S(format!("].{}", sw))]), ty, comps.len() as u8);
                        return coerce(ex, want);
                    }
                }
                self.raw_cbs.insert(slot);
                let ex = Ex::new(Node::Fmt(vec![Piece::S(format!("cb{}[", slot)), Piece::E(ix), Piece::S(format!("].{}", sw))]), Ty::F, comps.len() as u8);
                coerce(ex, want)
            }
        }
    }

    /// Read an operand as `want` for the destination components `dc` (selecting swizzle positions).
    fn read(&mut self, o: &Operand, dc: &[u8], want: Ty) -> Ex {
        let want = if want == Ty::Any && (o.abs || o.neg) { Ty::F } else { want };
        let sw = o.swizzle();
        let comps: Vec<u8> = dc.iter().map(|&c| sw[c as usize & 3]).collect();
        let w = comps.len();
        let mut e = match o.ty {
            0 => {
                let reg = o.index(0).unwrap_or(0);
                let want_eff = if want == Ty::Any { self.rty(reg) } else { want };
                let id = self.reads.len();
                self.reads.push(ReadInfo { reg, comps: comps.clone(), want: want_eff });
                Ex::new(Node::Reg(id), want_eff, w as u8)
            }
            4 => {
                let v: Vec<u32> = comps.iter().map(|&c| if o.imm.len() == 1 { o.imm[0] } else { o.imm.get(c as usize).copied().unwrap_or(0) }).collect();
                lit(v, Ty::Any)
            }
            1 => {
                if o.idx.len() >= 2 {
                    let (pre, vtx) = self.input_prefix(o, true);
                    let reg = o.index(1).unwrap_or(0);
                    self.sig_read(0, reg, &comps, vtx, &pre, want)
                } else {
                    let reg = o.index(0).unwrap_or(0);
                    self.sig_read(0, reg, &comps, None, "i", want)
                }
            }
            25 => {
                let (pre, vtx) = self.input_prefix(o, true);
                let reg = o.index(1).unwrap_or(0);
                self.sig_read(0, reg, &comps, vtx, &pre, want)
            }
            26 => {
                self.impure = true;
                let (pre, vtx) = self.input_prefix(o, false);
                let reg = o.index(1).unwrap_or(0);
                self.sig_read(1, reg, &comps, vtx, &pre, want)
            }
            27 => {
                let reg = o.index(0).unwrap_or(0);
                self.sig_read(2, reg, &comps, None, "pc", want)
            }
            2 => {
                self.impure = true;
                let reg = o.index(0).unwrap_or(0);
                let k = if self.phase >= 1 { 2 } else { 1 };
                self.sig_read(k, reg, &comps, None, "o", want)
            }
            8 => self.cb_read(o, &comps, want),
            9 => {
                self.uses_icb = true;
                let ix = o.idx.first().cloned().unwrap_or(Idx::Imm(0));
                let ixe = self.idx_ex(&ix);
                let ty = self.icb_ty();
                coerce(Ex::new(Node::Fmt(vec![Piece::S("icb[".into()), Piece::E(ixe), Piece::S(format!("].{}", letters(&comps)))]), ty, w as u8), want)
            }
            3 => {
                self.impure = true;
                let n = o.index(0).unwrap_or(0);
                let ix = o.idx.get(1).cloned().unwrap_or(Idx::Imm(0));
                let ixe = self.idx_ex(&ix);
                coerce(Ex::new(Node::Fmt(vec![Piece::S(format!("x{}[", n)), Piece::E(ixe), Piece::S(format!("].{}", letters(&comps)))]), Ty::F, w as u8), want)
            }
            t @ (11 | 22 | 23 | 24 | 28 | 32..=37) => {
                let (name, sem, ty, nw) = match t {
                    11 => ("vPrim", "SV_PrimitiveID", Ty::U, 1),
                    22 => ("vOutputControlPointID", "SV_OutputControlPointID", Ty::U, 1),
                    23 => ("vForkInstanceID", "SV_InstanceID", Ty::U, 1),
                    24 => ("vJoinInstanceID", "SV_InstanceID", Ty::U, 1),
                    28 => ("vDomain", "SV_DomainLocation", Ty::F, 3),
                    32 => ("vThreadID", "SV_DispatchThreadID", Ty::U, 3),
                    33 => ("vThreadGroupID", "SV_GroupID", Ty::U, 3),
                    34 => ("vThreadIDInGroup", "SV_GroupThreadID", Ty::U, 3),
                    35 => ("vCoverage", "SV_Coverage", Ty::U, 1),
                    36 => ("vThreadIDInGroupFlattened", "SV_GroupIndex", Ty::U, 1),
                    _ => ("vGSInstanceID", "SV_GSInstanceID", Ty::U, 1),
                };
                self.builtins.insert(name.to_string(), format!("{} {} : {}", ty.vec(nw), name, sem));
                let text = if nw == 1 { name.to_string() } else { format!("{}.{}", name, letters(&comps)) };
                coerce(Ex::raw(text, ty, w as u8), want)
            }
            t => {
                self.warn(&format!("opnd{}", t), format!("operand type {} is not supported as a source", t));
                Ex::raw(format!("operand{}", t), Ty::F, w as u8)
            }
        };
        if o.abs || o.neg {
            let hv = if want == Ty::Any { Ty::F } else { want };
            e = coerce(e, hv);
            if o.abs {
                e = Ex::call("abs", vec![e], hv, w as u8);
            }
            if o.neg {
                e = Ex::un("-", e, hv);
            }
        }
        coerce(e, want)
    }

    fn icb_ty(&self) -> Ty {
        // decided together with the declaration in `icb_decl`
        if self.icb_is_float() {
            Ty::F
        } else {
            Ty::U
        }
    }
    fn icb_data(&self) -> Option<&'a Vec<u32>> {
        self.p.instrs.iter().find_map(|i| match &i.custom {
            Some((3, d)) => Some(d),
            _ => None,
        })
    }
    fn icb_is_float(&self) -> bool {
        match self.icb_data() {
            Some(d) => d.iter().all(|&v| v == 0 || ((v >> 23) & 0xff >= 0x40 && (v >> 23) & 0xff < 0xc0)),
            None => true,
        }
    }

    // ---------------------------------------------------------------- operand writing

    fn write(&mut self, o: &Operand, e: Ex, out: &mut Vec<Stmt>) {
        let mask = o.mask();
        let dc = comps_of(mask);
        let pure = !std::mem::take(&mut self.impure);
        match o.ty {
            0 => {
                let reg = o.index(0).unwrap_or(0);
                if self.votes.len() <= reg as usize {
                    self.votes.resize(reg as usize + 1, [0; 3]);
                }
                match e.ty {
                    Ty::F => self.votes[reg as usize][0] += dc.len() as u32,
                    Ty::I => self.votes[reg as usize][1] += dc.len() as u32,
                    Ty::U => self.votes[reg as usize][2] += dc.len() as u32,
                    Ty::Any => {}
                }
                out.push(Stmt::Assign { reg, mask, e, pure, rid1: usize::MAX });
            }
            13 => {}
            2 | 12 | 15 | 38 | 39 => {
                let sig: &[SigElem] = if self.phase >= 1 { &self.p.psg } else { &self.p.osg };
                if o.ty == 2 {
                    let reg = o.idx.last().and_then(|i| if let Idx::Imm(v) = i { Some(*v) } else { None }).unwrap_or(0);
                    // group destination components by field
                    let mut groups: Vec<(String, Vec<(u8, u8)>, Ty, u8)> = Vec::new(); // (field, [(src pos, field comp)], ty, width)
                    for (k, &c) in dc.iter().enumerate() {
                        let (name, fc, ty, fw) = match find_field(sig, reg, c) {
                            Some(f) => (f.name, c - f.lo.min(c), f.ty, f.width),
                            None => (format!("v{}", reg), c, Ty::F, 4),
                        };
                        match groups.last_mut() {
                            Some(g) if g.0 == name => g.1.push((k as u8, fc)),
                            _ => groups.push((name, vec![(k as u8, fc)], ty, fw)),
                        }
                    }
                    let pre = if o.idx.len() >= 2 {
                        let ix = self.idx_ex(&o.idx[0].clone());
                        Some(ix)
                    } else {
                        None
                    };
                    for (name, pos, ty, fw) in groups {
                        let src: Vec<u8> = pos.iter().map(|p| p.0).collect();
                        let fcs: Vec<u8> = pos.iter().map(|p| p.1).collect();
                        let rhs = coerce(swz(e.clone(), &src), ty);
                        let sw = if fcs.len() as u8 == fw && fcs.iter().enumerate().all(|(i, &c)| c as usize == i) { String::new() } else { format!(".{}", letters(&fcs)) };
                        let lhs = match &pre {
                            Some(ix) => Ex::new(Node::Fmt(vec![Piece::S("o[".into()), Piece::E(ix.clone()), Piece::S(format!("].{}{}", name, sw))]), ty, fcs.len() as u8),
                            None => Ex::raw(format!("o.{}{}", name, sw), ty, fcs.len() as u8),
                        };
                        out.push(Stmt::Eff(Ex::bin("=", 0, lhs, rhs, ty)));
                    }
                } else {
                    let (nm, want_name) = match o.ty {
                        12 => ("SV_Depth", 0u32),
                        15 => ("SV_Coverage", 0),
                        38 => ("SV_DepthGreaterEqual", 0),
                        _ => ("SV_DepthLessEqual", 0),
                    };
                    let _ = want_name;
                    let ty = if o.ty == 15 { Ty::U } else { Ty::F };
                    let fname = self.p.osg.iter().find(|s| s.reg == u32::MAX).map(field_name).unwrap_or_else(|| nm.to_string());
                    let lhs = Ex::raw(format!("o.{}", fname), ty, 1);
                    out.push(Stmt::Eff(Ex::bin("=", 0, lhs, coerce(swz(e, &[0]), ty), ty)));
                }
            }
            3 => {
                let n = o.index(0).unwrap_or(0);
                let ix = o.idx.get(1).cloned().unwrap_or(Idx::Imm(0));
                let ixe = self.idx_ex(&ix);
                let full = mask == 0xf;
                let sw = if full { String::new() } else { format!(".{}", letters(&dc)) };
                let lhs = Ex::new(Node::Fmt(vec![Piece::S(format!("x{}[", n)), Piece::E(ixe), Piece::S(format!("]{}", sw))]), Ty::F, dc.len() as u8);
                out.push(Stmt::Eff(Ex::bin("=", 0, lhs, coerce(e, Ty::F), Ty::F)));
            }
            31 => {
                let n = o.index(0).unwrap_or(0);
                let ix = o.idx.get(1).cloned().unwrap_or(Idx::Imm(0));
                let ixe = self.idx_ex(&ix);
                let lhs = Ex::new(Node::Fmt(vec![Piece::S(format!("g{}[", n)), Piece::E(ixe), Piece::S(format!("].{}", letters(&dc)))]), Ty::U, dc.len() as u8);
                out.push(Stmt::Eff(Ex::bin("=", 0, lhs, coerce(e, Ty::U), Ty::U)));
            }
            t => {
                self.warn(&format!("dst{}", t), format!("operand type {} is not supported as a destination", t));
                out.push(Stmt::Comment(format!("write to unsupported operand type {} dropped", t)));
            }
        }
    }

    // ---------------------------------------------------------------- instructions

    fn src(&mut self, ins: &Instr, k: usize, dc: &[u8], ty: Ty) -> Ex {
        match ins.ops.get(k) {
            Some(o) => self.read(o, dc, ty),
            None => lit(vec![0; dc.len().max(1)], ty),
        }
    }

    fn tex_expr_name(&self, o: &Operand) -> String {
        self.tex_name(o.index(0).unwrap_or(0))
    }

    fn offset_arg(&self, ins: &Instr, ng: usize) -> Option<Ex> {
        if !ins.has_offset {
            return None;
        }
        let v: Vec<u32> = ins.offset.iter().take(ng.max(1)).map(|&x| x as i32 as u32).collect();
        Some(lit(v, Ty::I))
    }

    fn instr(&mut self, ins: &Instr, out: &mut Vec<Stmt>) {
        use Ty::*;
        let op = ins.op;
        let nd = ndst(op);
        let dc: Vec<u8> = ins.ops.first().filter(|_| nd >= 1).map(|o| comps_of(o.mask())).unwrap_or_default();
        let w = dc.len() as u8;
        self.impure = false;
        macro_rules! s {
            ($k:expr, $t:expr) => {
                self.src(ins, nd + $k, &dc, $t)
            };
        }
        macro_rules! bin {
            ($o:expr, $p:expr, $t:expr) => {{
                let a = s!(0, $t);
                let b = s!(1, $t);
                Ex::bin($o, $p, a, b, $t)
            }};
        }
        macro_rules! un {
            ($n:expr, $t:expr) => {{
                let a = s!(0, $t);
                Ex::call($n, vec![a], $t, w)
            }};
        }
        macro_rules! cmp {
            ($o:expr, $t:expr) => {{
                let a = s!(0, $t);
                let b = s!(1, $t);
                Ex::new(Node::Cmp($o, Box::new(a), Box::new(b)), U, w)
            }};
        }
        macro_rules! fin {
            ($e:expr) => {{
                let mut e = $e;
                if ins.sat {
                    e = Ex::call("saturate", vec![coerce(e, F)], F, w);
                }
                if let Some(d) = ins.ops.first() {
                    self.write(d, e, out);
                }
            }};
        }
        match op {
            0 => fin!(bin!("+", 60, F)),
            56 => fin!(bin!("*", 70, F)),
            14 => fin!(bin!("/", 70, F)),
            51 => fin!({
                let a = s!(0, F);
                let b = s!(1, F);
                Ex::call("min", vec![a, b], F, w)
            }),
            52 => fin!({
                let a = s!(0, F);
                let b = s!(1, F);
                Ex::call("max", vec![a, b], F, w)
            }),
            50 => fin!({
                let a = s!(0, F);
                let b = s!(1, F);
                let c = s!(2, F);
                Ex::bin("+", 60, Ex::bin("*", 70, a, b, F), c, F)
            }),
            15..=17 => {
                let n = (op - 13) as usize;
                let dd: Vec<u8> = (0..n as u8).collect();
                let a = self.src(ins, nd, &dd, F);
                let b = self.src(ins, nd + 1, &dd, F);
                let e = if n == 1 { Ex::bin("*", 70, a, b, F) } else { Ex::call("dot", vec![a, b], F, 1) };
                fin!(e)
            }
            25 => fin!(un!("exp2", F)),
            26 => fin!(un!("frac", F)),
            47 => fin!(un!("log2", F)),
            64 => fin!(un!("round", F)),
            65 => fin!(un!("floor", F)),
            66 => fin!(un!("ceil", F)),
            67 => fin!(un!("trunc", F)),
            68 => fin!(un!("rsqrt", F)),
            75 => fin!(un!("sqrt", F)),
            11 => fin!(un!("ddx", F)),
            12 => fin!(un!("ddy", F)),
            122 => fin!(un!("ddx_coarse", F)),
            123 => fin!(un!("ddx_fine", F)),
            124 => fin!(un!("ddy_coarse", F)),
            125 => fin!(un!("ddy_fine", F)),
            129 => fin!({
                let a = s!(0, F);
                Ex::bin("/", 70, lit(vec![1.0f32.to_bits()], F), a, F)
            }),
            24 => fin!(cmp!("==", F)),
            57 => fin!(cmp!("!=", F)),
            49 => fin!(cmp!("<", F)),
            29 => fin!(cmp!(">=", F)),
            32 => fin!(cmp!("==", I)),
            39 => fin!(cmp!("!=", I)),
            34 => fin!(cmp!("<", I)),
            33 => fin!(cmp!(">=", I)),
            79 => fin!(cmp!("<", U)),
            80 => fin!(cmp!(">=", U)),
            30 => fin!(bin!("+", 60, I)),
            36 => fin!({
                let a = s!(0, I);
                let b = s!(1, I);
                Ex::call("max", vec![a, b], I, w)
            }),
            37 => fin!({
                let a = s!(0, I);
                let b = s!(1, I);
                Ex::call("min", vec![a, b], I, w)
            }),
            83 => fin!({
                let a = s!(0, U);
                let b = s!(1, U);
                Ex::call("max", vec![a, b], U, w)
            }),
            84 => fin!({
                let a = s!(0, U);
                let b = s!(1, U);
                Ex::call("min", vec![a, b], U, w)
            }),
            40 => fin!({
                let a = s!(0, I);
                Ex::un("-", a, I)
            }),
            35 | 82 => {
                let t = if op == 35 { I } else { U };
                fin!({
                    let a = s!(0, t);
                    let b = s!(1, t);
                    let c = s!(2, t);
                    Ex::bin("+", 60, Ex::bin("*", 70, a, b, t), c, t)
                })
            }
            38 | 81 => {
                // dst0 = high word, dst1 = low word
                let t = if op == 38 { I } else { U };
                let a = self.src(ins, 2, &comps_of(ins.ops[1].mask()), t);
                let b = self.src(ins, 3, &comps_of(ins.ops[1].mask()), t);
                if ins.ops.get(1).map(|o| o.ty != 13).unwrap_or(false) {
                    self.impure = false;
                    let e = Ex::bin("*", 70, a.clone(), b.clone(), t);
                    let d1 = ins.ops[1].clone();
                    self.write(&d1, e, out);
                }
                if ins.ops.first().map(|o| o.ty != 13).unwrap_or(false) {
                    let dc0 = comps_of(ins.ops[0].mask());
                    let a0 = self.src(ins, 2, &dc0, t);
                    let b0 = self.src(ins, 3, &dc0, t);
                    let e = Ex::call(if op == 38 { "imul_hi" } else { "umul_hi" }, vec![a0, b0], t, dc0.len() as u8);
                    self.impure = false;
                    let d0 = ins.ops[0].clone();
                    self.write(&d0, e, out);
                }
            }
            41 => fin!({
                let a = s!(0, I);
                let b = s!(1, U);
                let b = match &b.n {
                    Node::Lit(v) if v.iter().all(|&x| x < 32) => b,
                    _ => Ex::bin("&", 40, b, lit(vec![31], U), U),
                };
                Ex::bin("<<", 55, a, b, I)
            }),
            42 | 85 => {
                let t = if op == 42 { I } else { U };
                fin!({
                    let a = s!(0, t);
                    let b = s!(1, U);
                    let b = match &b.n {
                        Node::Lit(v) if v.iter().all(|&x| x < 32) => b,
                        _ => Ex::bin("&", 40, b, lit(vec![31], U), U),
                    };
                    Ex::bin(">>", 55, a, b, t)
                })
            }
            1 => fin!(bin!("&", 40, U)),
            60 => fin!(bin!("|", 36, U)),
            87 => fin!(bin!("^", 38, U)),
            59 => fin!({
                let a = s!(0, U);
                Ex::un("~", a, U)
            }),
            78 => {
                // udiv: dst0 quotient, dst1 remainder
                let t = U;
                if ins.ops.first().map(|o| o.ty != 13).unwrap_or(false) {
                    let d0 = ins.ops[0].clone();
                    let c0 = comps_of(d0.mask());
                    let a = self.src(ins, 2, &c0, t);
                    let b = self.src(ins, 3, &c0, t);
                    self.impure = false;
                    self.write(&d0, Ex::bin("/", 70, a, b, t), out);
                }
                if ins.ops.get(1).map(|o| o.ty != 13).unwrap_or(false) {
                    let d1 = ins.ops[1].clone();
                    let c1 = comps_of(d1.mask());
                    let a = self.src(ins, 2, &c1, t);
                    let b = self.src(ins, 3, &c1, t);
                    self.impure = false;
                    self.write(&d1, Ex::bin("%", 70, a, b, t), out);
                }
            }
            132 | 133 => {
                let (sum_op, carry_name) = if op == 132 { ("+", "uaddc_carry") } else { ("-", "usubb_borrow") };
                if ins.ops.first().map(|o| o.ty != 13).unwrap_or(false) {
                    let d0 = ins.ops[0].clone();
                    let c0 = comps_of(d0.mask());
                    let a = self.src(ins, 2, &c0, U);
                    let b = self.src(ins, 3, &c0, U);
                    self.impure = false;
                    self.write(&d0, Ex::bin(sum_op, 60, a, b, U), out);
                }
                if ins.ops.get(1).map(|o| o.ty != 13).unwrap_or(false) {
                    let d1 = ins.ops[1].clone();
                    let c1 = comps_of(d1.mask());
                    let a = self.src(ins, 2, &c1, U);
                    let b = self.src(ins, 3, &c1, U);
                    self.impure = false;
                    self.write(&d1, Ex::call(carry_name, vec![a, b], U, c1.len() as u8), out);
                }
            }
            27 => fin!({
                let a = s!(0, F);
                Ex::call("int", vec![a], I, w)
            }),
            28 => fin!({
                let a = s!(0, F);
                Ex::call("uint", vec![a], U, w)
            }),
            43 => fin!({
                let a = s!(0, I);
                Ex::call("float", vec![a], F, w)
            }),
            86 => fin!({
                let a = s!(0, U);
                Ex::call("float", vec![a], F, w)
            }),
            130 => fin!({
                let a = s!(0, F);
                Ex::call("f32tof16", vec![a], U, w)
            }),
            131 => fin!({
                let a = s!(0, U);
                Ex::call("f16tof32", vec![a], F, w)
            }),
            134 => fin!(un!("countbits", U)),
            135 => fin!(un!("firstbithigh", U)),
            136 => fin!(un!("firstbitlow", U)),
            137 => fin!({
                let a = s!(0, I);
                Ex::call("firstbithigh", vec![a], I, w)
            }),
            141 => fin!(un!("reversebits", U)),
            138 | 139 | 140 => fin!({
                let n = if op == 140 { 4 } else { 3 };
                let args: Vec<Ex> = (0..n).map(|k| self.src(ins, nd + k, &dc, if op == 139 && k == n - 1 { I } else { U })).collect();
                let t = if op == 139 { I } else { U };
                Ex::call(["ubfe", "ibfe", "bfi"][(op - 138) as usize], args, t, w)
            }),
            54 => {
                let a = s!(0, Any);
                fin!(a)
            }
            55 => fin!({
                let c = coerce(s!(0, U), U);
                let a = s!(1, Any);
                let b = s!(2, Any);
                let t = if a.ty != Any { a.ty } else { b.ty };
                let (a, b) = (coerce(a, t), coerce(b, t));
                Ex::new(Node::Cond(Box::new(c), Box::new(a), Box::new(b)), t, w)
            }),
            142 => {
                // swapc dst0, dst1, cond, a, b : dst0 = cond ? b : a ; dst1 = cond ? a : b
                for (k, flip) in [(0usize, true), (1, false)] {
                    if ins.ops.get(k).map(|o| o.ty != 13).unwrap_or(false) {
                        let d = ins.ops[k].clone();
                        let cc = comps_of(d.mask());
                        let c = coerce(self.src(ins, 2, &cc, U), U);
                        let a = self.src(ins, 3, &cc, Any);
                        let b = self.src(ins, 4, &cc, Any);
                        let t = if a.ty != Any { a.ty } else { b.ty };
                        let (a, b) = (coerce(a, t), coerce(b, t));
                        let (x, y) = if flip { (b, a) } else { (a, b) };
                        self.impure = false;
                        self.write(&d, Ex::new(Node::Cond(Box::new(c), Box::new(x), Box::new(y)), t, cc.len() as u8), out);
                    }
                }
            }
            77 => {
                for (k, f) in [(0usize, "sin"), (1, "cos")] {
                    if ins.ops.get(k).map(|o| o.ty != 13).unwrap_or(false) {
                        let d = ins.ops[k].clone();
                        let cc = comps_of(d.mask());
                        let a = self.src(ins, 2, &cc, F);
                        self.impure = false;
                        self.write(&d, Ex::call(f, vec![a], F, cc.len() as u8), out);
                    }
                }
            }
            69..=74 | 108 | 109 | 126 | 127 | 128 => self.sample_like(ins, out),
            45 | 46 | 61 | 111 | 110 | 121 => self.fetch_like(ins, out),
            163..=189 => self.mem_like(ins, out),
            203..=205 => {
                // eval_snapped / eval_sample_index / eval_centroid
                let d = ins.ops[0].clone();
                let cc = comps_of(d.mask());
                self.impure = false;
                let inp = self.src(ins, 1, &cc, F);
                let e = match op {
                    203 => {
                        let off = self.src(ins, 2, &[0, 1], I);
                        Ex::call("EvaluateAttributeSnapped", vec![inp, off], F, w)
                    }
                    204 => {
                        let idx = self.src(ins, 2, &[0], I);
                        Ex::call("EvaluateAttributeAtSample", vec![inp, idx], F, w)
                    }
                    _ => Ex::call("EvaluateAttributeAtCentroid", vec![inp], F, w),
                };
                self.write(&d, e, out);
            }
            // ------------------------------------------------ control flow handled by the caller
            13 | 31 | 3 | 8 | 63 | 2 | 7 | 62 | 48 | 18 | 21 | 22 | 23 | 76 | 6 | 10 | 58 => {}
            9 | 19 | 20 | 117 | 118 | 119 => {
                self.gs_stream = true;
                let sn = ins.ops.first().and_then(|o| o.index(0)).unwrap_or(0);
                let sname = if op >= 117 && sn != 0 { format!("stream{}", sn) } else { "stream".to_string() };
                let call = match op {
                    9 | 118 => format!("{}.RestartStrip()", sname),
                    19 | 117 => format!("{}.Append(o)", sname),
                    _ => format!("{}.Append(o); {}.RestartStrip()", sname, sname),
                };
                out.push(Stmt::Eff(Ex::raw(call, U, 1)));
            }
            190 => {
                let c = ins.control;
                let mut n = String::from("Sync");
                if c & 8 != 0 {
                    n.push_str("_uglobal");
                }
                if c & 4 != 0 {
                    n.push_str("_ugroup");
                }
                if c & 2 != 0 {
                    n.push_str("_g");
                }
                if c & 1 != 0 {
                    n.push_str("_t");
                }
                out.push(Stmt::Eff(Ex::raw(format!("{}()", n), U, 1)));
            }
            4 | 5 | 44 => {
                self.warn("call", "subroutine call/label instructions are printed as comments".into());
                out.push(Stmt::Comment(instr_text(ins, &self.res)));
            }
            _ => {
                // generic fallback: pseudo-function named like the instruction, kept impure so it is never folded
                let name = opname(op).unwrap_or("unknown").to_string();
                if opname(op).is_some() && !is_decl(op) {
                    self.warn(&format!("op{}", op), format!("{}: no HLSL rendering, printed as a pseudo-call", name));
                }
                let mut args = Vec::new();
                let dcs = if nd >= 1 { dc.clone() } else { vec![0, 1, 2, 3] };
                for k in nd..ins.ops.len() {
                    if matches!(ins.ops[k].ty, 6 | 7 | 30 | 31) {
                        args.push(Ex::raw(crate::dxbc::disasm::operand_str(&ins.ops[k], crate::dxbc::disasm::Kind::X), Any, 1));
                    } else {
                        args.push(self.src(ins, k, &dcs, Any));
                    }
                }
                let e = Ex::call(&name, args, Any, w.max(1));
                self.impure = true;
                if nd >= 1 {
                    for k in 0..nd {
                        let d = ins.ops[k].clone();
                        let e2 = if k == 0 { e.clone() } else { Ex::call(&format!("{}_{}", name, k), vec![], Any, 1) };
                        let e2 = coerce(e2, F);
                        self.impure = true;
                        self.write(&d, e2, out);
                    }
                } else if !is_decl(op) {
                    out.push(Stmt::Eff(e));
                }
            }
        }
    }

    fn sample_like(&mut self, ins: &Instr, out: &mut Vec<Stmt>) {
        use Ty::*;
        let op = ins.op;
        let po = op == 127 || op == 128;
        let (ti_idx, si_idx) = if po { (3, 4) } else { (2, 3) };
        let (Some(dst), Some(tex), Some(smp)) = (ins.ops.first(), ins.ops.get(ti_idx), ins.ops.get(si_idx)) else {
            self.warn("sample-short", format!("{}: too few operands", opname(op).unwrap_or("?")));
            return;
        };
        let dc = comps_of(dst.mask());
        let ti = self.tex_info(tex.index(0).unwrap_or(0));
        let tname = self.tex_expr_name(tex);
        let sname = self.smp_name(smp.index(0).unwrap_or(0));
        let nc = ti.ncoord;
        let cc: Vec<u8> = (0..nc as u8).collect();
        let coord = self.src(ins, 1, &cc, F);
        let mut args = vec![Ex::raw(sname, U, 1), coord];
        let ts = tex.swizzle();
        let pos: Vec<u8> = dc.iter().map(|&c| ts[c as usize & 3]).collect();
        let sc = |s: &mut Self, k: usize, n: usize, t: Ty| -> Ex {
            let c: Vec<u8> = (0..n as u8).collect();
            s.src(ins, k, &c, t)
        };
        let mut rty = ti.ret;
        let (method, rw) = match op {
            69 => ("Sample".to_string(), 4),
            70 => {
                let r = sc(self, 4, 1, F);
                args.push(r);
                rty = F;
                ("SampleCmp".to_string(), 1)
            }
            71 => {
                let r = sc(self, 4, 1, F);
                args.push(r);
                rty = F;
                ("SampleCmpLevelZero".to_string(), 1)
            }
            72 => {
                let r = sc(self, 4, 1, F);
                args.push(r);
                ("SampleLevel".to_string(), 4)
            }
            73 => {
                let a = sc(self, 4, ti.ngrad, F);
                let b = sc(self, 5, ti.ngrad, F);
                args.push(a);
                args.push(b);
                ("SampleGrad".to_string(), 4)
            }
            74 => {
                let r = sc(self, 4, 1, F);
                args.push(r);
                ("SampleBias".to_string(), 4)
            }
            108 => ("CalculateLevelOfDetail".to_string(), 4),
            109 | 126 | 127 | 128 => {
                let comp = smp.swizzle()[0] as usize & 3;
                let base = ["GatherRed", "GatherGreen", "GatherBlue", "GatherAlpha"][comp];
                let cmp = op == 126 || op == 128;
                if po {
                    let o = sc(self, 2, ti.ngrad.min(2), I);
                    args.clear();
                    let sname2 = self.smp_name(smp.index(0).unwrap_or(0));
                    let coord2 = self.src(ins, 1, &cc, F);
                    args.push(Ex::raw(sname2, U, 1));
                    args.push(coord2);
                    if cmp {
                        let r = sc(self, 5, 1, F);
                        args.push(r);
                    }
                    args.push(o);
                } else if cmp {
                    let r = sc(self, 4, 1, F);
                    args.push(r);
                }
                (if cmp { format!("GatherCmp{}", &base[6..]) } else { base.to_string() }, 4)
            }
            _ => ("Sample".to_string(), 4),
        };
        if !po {
            if let Some(o) = self.offset_arg(ins, ti.ngrad) {
                args.push(o);
            }
        }
        let e = if op == 108 {
            let c = args.remove(1);
            let s0 = args.remove(0);
            let a = Ex::call(&format!("{}.CalculateLevelOfDetail", tname), vec![s0.clone(), c.clone()], F, 1);
            let b = Ex::call(&format!("{}.CalculateLevelOfDetailUnclamped", tname), vec![s0, c], F, 1);
            let items: Vec<Ex> = pos.iter().map(|&p| if p == 0 { a.clone() } else if p == 1 { b.clone() } else { lit(vec![0], F) }).collect();
            let wn = items.len() as u8;
            if wn == 1 { items.into_iter().next().unwrap() } else { Ex::new(Node::Vecn(items), F, wn) }
        } else {
            let call = Ex::call(&format!("{}.{}", tname, method), args, rty, rw);
            swz(call, if rw == 1 { &[0u8][..] } else { &pos })
        };
        // a 1-wide result replicated over the mask
        self.impure = false;
        let d = dst.clone();
        self.write(&d, e, out);
    }

    fn fetch_like(&mut self, ins: &Instr, out: &mut Vec<Stmt>) {
        use Ty::*;
        let op = ins.op;
        let Some(dst) = ins.ops.first() else { return };
        let dc = comps_of(dst.mask());
        match op {
            45 | 46 => {
                let (Some(tex),) = (ins.ops.get(2),) else { return };
                let ti = self.tex_info(tex.index(0).unwrap_or(0));
                let tname = self.tex_expr_name(tex);
                let n = if ti.dim == 1 || ti.dim == 11 { 1 } else if ti.ms { ti.ncoord } else { ti.ncoord + 1 };
                let cc: Vec<u8> = (0..n as u8).collect();
                let addr = self.src(ins, 1, &cc, I);
                let mut args = vec![addr];
                if op == 46 {
                    let sidx = self.src(ins, 3, &[0], I);
                    args.push(sidx);
                }
                if let Some(o) = self.offset_arg(ins, ti.ngrad) {
                    args.push(o);
                }
                let ts = tex.swizzle();
                let pos: Vec<u8> = dc.iter().map(|&c| ts[c as usize & 3]).collect();
                let call = Ex::call(&format!("{}.Load", tname), args, ti.ret, 4);
                self.impure = false;
                let d = dst.clone();
                self.write(&d, swz(call, &pos), out);
            }
            61 => {
                let Some(tex) = ins.ops.get(2) else { return };
                let tname = self.tex_expr_name(tex);
                let ts = tex.swizzle();
                let pos: Vec<u8> = dc.iter().map(|&c| ts[c as usize & 3]).collect();
                let mip = self.src(ins, 1, &[0], U);
                let kind = ["", "_rcpFloat", "_uint"][(ins.control & 3).min(2) as usize];
                let ty = if kind == "_uint" { U } else { F };
                let call = Ex::call(&format!("resinfo{}", kind), vec![Ex::raw(tname, U, 1), mip], ty, 4);
                self.impure = false;
                let d = dst.clone();
                self.write(&d, swz(call, &pos), out);
            }
            121 => {
                let Some(tex) = ins.ops.get(1) else { return };
                let tname = self.tex_expr_name(tex);
                let call = Ex::call("bufinfo", vec![Ex::raw(tname, U, 1)], U, 1);
                self.impure = false;
                let d = dst.clone();
                self.write(&d, call, out);
            }
            111 => {
                let call = Ex::call("sampleinfo", vec![], Any, 1);
                self.impure = false;
                let d = dst.clone();
                self.write(&d, call, out);
            }
            _ => {
                let idx = self.src(ins, 2, &[0], I);
                let call = Ex::call("samplepos", vec![idx], F, 4);
                self.impure = false;
                let ts = ins.ops.get(1).map(|o| o.swizzle()).unwrap_or([0, 1, 2, 3]);
                let pos: Vec<u8> = dc.iter().map(|&c| ts[c as usize & 3]).collect();
                let d = dst.clone();
                self.write(&d, swz(call, &pos), out);
            }
        }
    }

    fn res_expr(&self, o: &Operand) -> String {
        let n = o.index(0).unwrap_or(0);
        match o.ty {
            7 => self.tex_name(n),
            30 => self.uav_name(n),
            31 => format!("g{}", n),
            _ => crate::dxbc::disasm::operand_str(o, crate::dxbc::disasm::Kind::X),
        }
    }

    fn mem_like(&mut self, ins: &Instr, out: &mut Vec<Stmt>) {
        use Ty::*;
        let op = ins.op;
        let uv = |s: &Self, o: &Operand| -> (String, u8) {
            let n = o.index(0).unwrap_or(0);
            let k = if o.ty == 31 { if s.tgsm_structured.contains(&n) { 2 } else { 1 } } else { s.uav_kind.get(&n).copied().unwrap_or(0) };
            (s.res_expr(o), k)
        };
        match op {
            163 => {
                // ld_uav_typed dst, addr, u
                let (Some(dst), Some(u)) = (ins.ops.first(), ins.ops.get(2)) else { return };
                let n = u.index(0).unwrap_or(0);
                let (dim, ret) = self.uavs.get(&n).copied().unwrap_or((3, 0x5555));
                let (nc, _) = dim_coords(dim);
                let cc: Vec<u8> = (0..nc as u8).collect();
                let addr = self.src(ins, 1, &cc, U);
                let name = self.uav_name(n);
                let rt = ret_ty(ret);
                let ex = Ex::new(Node::Fmt(vec![Piece::S(format!("{}[", name)), Piece::E(addr), Piece::S("]".into())]), rt, 4);
                let ts = u.swizzle();
                let dc = comps_of(dst.mask());
                let pos: Vec<u8> = dc.iter().map(|&c| ts[c as usize & 3]).collect();
                self.impure = false;
                self.impure = true;
                let d = dst.clone();
                self.write(&d, swz(ex, &pos), out);
            }
            164 => {
                // store_uav_typed u, addr, value
                let (Some(u), Some(_)) = (ins.ops.first(), ins.ops.get(2)) else { return };
                let n = u.index(0).unwrap_or(0);
                let (dim, ret) = self.uavs.get(&n).copied().unwrap_or((3, 0x5555));
                let (nc, _) = dim_coords(dim);
                let cc: Vec<u8> = (0..nc as u8).collect();
                let addr = self.src(ins, 1, &cc, U);
                let rt = ret_ty(ret);
                let val = self.src(ins, 2, &[0, 1, 2, 3], rt);
                let lhs = Ex::new(Node::Fmt(vec![Piece::S(format!("{}[", self.uav_name(n))), Piece::E(addr), Piece::S("]".into())]), rt, 4);
                out.push(Stmt::Eff(Ex::bin("=", 0, lhs, val, rt)));
            }
            165 => {
                // ld_raw dst, byteaddr, res
                let (Some(dst), Some(r)) = (ins.ops.first(), ins.ops.get(2)) else { return };
                let dc = comps_of(dst.mask());
                let rs = r.swizzle();
                let pos: Vec<u8> = dc.iter().map(|&c| rs[c as usize & 3]).collect();
                let hi = pos.iter().copied().max().unwrap_or(0) + 1;
                let addr = self.src(ins, 1, &[0], U);
                let name = self.res_expr(r);
                let m = if hi == 1 { "Load".to_string() } else { format!("Load{}", hi) };
                let call = Ex::call(&format!("{}.{}", name, m), vec![addr], U, hi);
                self.impure = true;
                let d = dst.clone();
                self.write(&d, swz(call, &pos), out);
            }
            166 => {
                let (Some(r),) = (ins.ops.first(),) else { return };
                let dc = comps_of(r.mask());
                let addr = self.src(ins, 1, &[0], U);
                let val = self.src(ins, 2, &dc, U);
                let m = if dc.len() == 1 { "Store".to_string() } else { format!("Store{}", dc.len()) };
                out.push(Stmt::Eff(Ex::call(&format!("{}.{}", self.res_expr(r), m), vec![addr, val], U, 1)));
            }
            167 => {
                let (Some(dst), Some(r)) = (ins.ops.first(), ins.ops.get(3)) else { return };
                let dc = comps_of(dst.mask());
                let rs = r.swizzle();
                let pos: Vec<u8> = dc.iter().map(|&c| rs[c as usize & 3]).collect();
                let idx = self.src(ins, 1, &[0], U);
                let off = self.src(ins, 2, &[0], U);
                let call = Ex::call("LoadStructured", vec![Ex::raw(self.res_expr(r), U, 1), idx, off], U, 4);
                self.impure = true;
                let d = dst.clone();
                self.write(&d, swz(call, &pos), out);
            }
            168 => {
                let Some(r) = ins.ops.first() else { return };
                let dc = comps_of(r.mask());
                let idx = self.src(ins, 1, &[0], U);
                let off = self.src(ins, 2, &[0], U);
                let val = self.src(ins, 3, &dc, U);
                out.push(Stmt::Eff(Ex::call("StoreStructured", vec![Ex::raw(self.res_expr(r), U, 1), idx, off, val], U, 1)));
            }
            _ => {
                // atomics: atomic_*: (res, addr, [cmp,] value) ; imm_atomic_*: (dst, res, addr, [cmp,] value) ; alloc/consume: (dst, res)
                let imm = op >= 178;
                let k0 = if imm { 1 } else { 0 };
                let name = match op {
                    169 | 181 => "InterlockedAnd",
                    170 | 182 => "InterlockedOr",
                    171 | 183 => "InterlockedXor",
                    172 | 185 => if op == 172 { "InterlockedCompareStore" } else { "InterlockedCompareExchange" },
                    173 | 180 => "InterlockedAdd",
                    174 | 186 => "InterlockedMax",
                    175 | 187 => "InterlockedMin",
                    176 | 188 => "InterlockedMax",
                    177 | 189 => "InterlockedMin",
                    184 => "InterlockedExchange",
                    178 => "IncrementCounter",
                    179 => "DecrementCounter",
                    _ => "Interlocked",
                };
                let signed = matches!(op, 174 | 175 | 186 | 187);
                let t = if signed { I } else { U };
                let Some(r) = ins.ops.get(k0) else { return };
                let (rname, kind) = uv(self, r);
                if op == 178 || op == 179 {
                    let Some(d) = ins.ops.first() else { return };
                    let call = Ex::call(&format!("{}.{}", rname, name), vec![], U, 1);
                    self.impure = true;
                    let d = d.clone();
                    self.write(&d, call, out);
                    return;
                }
                let na = if r.ty == 31 { kind.max(1) } else { match kind { 0 => { let n = r.index(0).unwrap_or(0); dim_coords(self.uavs.get(&n).map(|x| x.0).unwrap_or(3)).0 as u8 } k => k.max(1) } };
                let cc: Vec<u8> = (0..na).collect();
                let addr = self.src(ins, k0 + 1, &cc, U);
                let target = Ex::new(Node::Fmt(vec![Piece::S(format!("{}[", rname)), Piece::E(addr), Piece::S("]".into())]), U, 1);
                let mut args = vec![target];
                let nv = ins.ops.len().saturating_sub(k0 + 2);
                for j in 0..nv {
                    let v = self.src(ins, k0 + 2 + j, &[0], t);
                    args.push(v);
                }
                let call = Ex::call(name, args, t, 1);
                self.impure = true;
                if imm {
                    let d = ins.ops[0].clone();
                    self.write(&d, call, out);
                } else {
                    out.push(Stmt::Eff(call));
                }
            }
        }
    }

    // ---------------------------------------------------------------- structured control flow

    fn translate(&mut self, instrs: &[&Instr]) -> Vec<Stmt> {
        enum Fk {
            Root,
            If(Ex, bool),
            Else(Ex, bool, Vec<Stmt>),
            Loop,
            Switch(Ex),
        }
        let mut stack: Vec<(Fk, Vec<Stmt>)> = vec![(Fk::Root, Vec::new())];
        macro_rules! cur {
            () => {
                stack.last_mut().unwrap().1
            };
        }
        for ins in instrs {
            let op = ins.op;
            match op {
                31 => {
                    self.impure = false;
                    let c = self.src(ins, 0, &[0], Ty::U);
                    stack.push((Fk::If(c, ins.test_nz), Vec::new()));
                }
                18 => {
                    let (k, body) = stack.pop().unwrap();
                    match k {
                        Fk::If(c, nz) => stack.push((Fk::Else(c, nz, body), Vec::new())),
                        other => {
                            self.warn("else", "`else` without `if`".into());
                            stack.push((other, body));
                        }
                    }
                }
                21 => {
                    if stack.len() > 1 {
                        let (k, body) = stack.pop().unwrap();
                        let st = match k {
                            Fk::If(c, nz) => Stmt::If { cond: c, nz, then: body, els: vec![] },
                            Fk::Else(c, nz, then) => Stmt::If { cond: c, nz, then, els: body },
                            other => {
                                self.warn("endif", "`endif` closes a different block".into());
                                stack.push((other, body));
                                continue;
                            }
                        };
                        cur!().push(st);
                    }
                }
                48 => stack.push((Fk::Loop, Vec::new())),
                22 => {
                    if stack.len() > 1 {
                        let (k, body) = stack.pop().unwrap();
                        if let Fk::Loop = k {
                            cur!().push(Stmt::Loop(body));
                        } else {
                            self.warn("endloop", "`endloop` closes a different block".into());
                            stack.push((k, body));
                        }
                    }
                }
                76 => {
                    self.impure = false;
                    let c = self.src(ins, 0, &[0], Ty::I);
                    stack.push((Fk::Switch(c), Vec::new()));
                }
                23 => {
                    if stack.len() > 1 {
                        let (k, body) = stack.pop().unwrap();
                        if let Fk::Switch(sel) = k {
                            cur!().push(Stmt::Switch { sel, body });
                        } else {
                            self.warn("endswitch", "`endswitch` closes a different block".into());
                            stack.push((k, body));
                        }
                    }
                }
                6 => {
                    let v = ins.ops.first().and_then(|o| o.imm.first().copied()).unwrap_or(0);
                    cur!().push(Stmt::Label(format!("case {}:", v as i32)));
                }
                10 => cur!().push(Stmt::Label("default:".into())),
                2 => cur!().push(Stmt::Jump { kind: "break", cond: None }),
                7 => cur!().push(Stmt::Jump { kind: "continue", cond: None }),
                62 => cur!().push(Stmt::Jump { kind: "return", cond: None }),
                3 | 8 | 63 | 13 => {
                    self.impure = false;
                    let c = self.src(ins, 0, &[0], Ty::U);
                    let kind = match op {
                        3 => "break",
                        8 => "continue",
                        63 => "return",
                        _ => "discard",
                    };
                    cur!().push(Stmt::Jump { kind, cond: Some((c, ins.test_nz)) });
                }
                _ => {
                    if is_decl(op) || ins.custom.is_some() {
                        continue;
                    }
                    if ins.bad {
                        self.warn("badinstr", format!("instruction {} could not be decoded; skipped", opname(op).unwrap_or("?")));
                        cur!().push(Stmt::Comment(instr_text(ins, &self.res)));
                        continue;
                    }
                    let mut v = Vec::new();
                    self.instr(ins, &mut v);
                    let end = self.reads.len();
                    for st in v.iter_mut() {
                        if let Stmt::Assign { rid1, .. } = st {
                            *rid1 = end;
                        }
                    }
                    cur!().extend(v);
                }
            }
        }
        while stack.len() > 1 {
            self.warn("unclosed", "unterminated control flow block at end of shader".into());
            let (k, body) = stack.pop().unwrap();
            let st = match k {
                Fk::If(c, nz) => Stmt::If { cond: c, nz, then: body, els: vec![] },
                Fk::Else(c, nz, then) => Stmt::If { cond: c, nz, then, els: body },
                Fk::Loop => Stmt::Loop(body),
                Fk::Switch(sel) => Stmt::Switch { sel, body },
                Fk::Root => break,
            };
            cur!().push(st);
        }
        stack.pop().map(|x| x.1).unwrap_or_default()
    }
}

// -------------------------------------------------------------------- folding and printing

fn heads<'s>(s: &'s Stmt, f: &mut dyn FnMut(&'s Ex)) {
    match s {
        Stmt::Assign { e, .. } => f(e),
        Stmt::Eff(e) => f(e),
        Stmt::If { cond, .. } => f(cond),
        Stmt::Switch { sel, .. } => f(sel),
        Stmt::Jump { cond: Some((c, _)), .. } => f(c),
        _ => {}
    }
}

fn heads_mut(s: &mut Stmt, f: &mut dyn FnMut(&mut Ex) -> bool) -> bool {
    match s {
        Stmt::Assign { e, .. } => f(e),
        Stmt::Eff(e) => f(e),
        Stmt::If { cond, .. } => f(cond),
        Stmt::Switch { sel, .. } => f(sel),
        Stmt::Jump { cond: Some((c, _)), .. } => f(c),
        _ => false,
    }
}

fn walk_stmts<'s>(list: &'s [Stmt], f: &mut dyn FnMut(&'s Stmt)) {
    for s in list {
        f(s);
        match s {
            Stmt::If { then, els, .. } => {
                walk_stmts(then, f);
                walk_stmts(els, f);
            }
            Stmt::Loop(b) | Stmt::Switch { body: b, .. } => walk_stmts(b, f),
            _ => {}
        }
    }
}

struct FoldInfo {
    occurrences: HashMap<usize, u32>,
    reads_by_comp: HashMap<(u32, u8), Vec<usize>>,
    writes: HashMap<(u32, u8), u32>,
}

fn analyse(list: &[Stmt], reads: &[ReadInfo]) -> FoldInfo {
    let mut fi = FoldInfo { occurrences: HashMap::new(), reads_by_comp: HashMap::new(), writes: HashMap::new() };
    walk_stmts(list, &mut |s| {
        heads(s, &mut |e| {
            walk_reads(e, &mut |id| {
                let c = fi.occurrences.entry(id).or_insert(0);
                *c += 1;
                if *c == 1 {
                    if let Some(r) = reads.get(id) {
                        for &k in &r.comps {
                            fi.reads_by_comp.entry((r.reg, k)).or_default().push(id);
                        }
                    }
                }
            })
        });
        if let Stmt::Assign { reg, mask, .. } = s {
            for c in 0..4u8 {
                if mask & (1 << c) != 0 {
                    *fi.writes.entry((*reg, c)).or_insert(0) += 1;
                }
            }
        }
    });
    fi
}

const WINDOW: usize = 96;

fn reads_comp(e: &Ex, reads: &[ReadInfo], reg: u32, mask: u8, out: &mut Vec<usize>) {
    walk_reads(e, &mut |rid| {
        if let Some(r) = reads.get(rid) {
            if r.reg == reg && r.comps.iter().any(|&c| mask & (1 << c) != 0) {
                out.push(rid);
            }
        }
    });
}

fn stmt_reads_comp(s: &Stmt, reads: &[ReadInfo], reg: u32, mask: u8) -> Vec<usize> {
    let mut v = Vec::new();
    heads(s, &mut |e| reads_comp(e, reads, reg, mask, &mut v));
    v
}

/// True when, starting at the beginning of `l`, component `bit` of `reg` is overwritten by a plain statement before anything
/// reads it or control flow could leave (conservative: any other statement kind answers false).
fn killed(l: &[Stmt], reads: &[ReadInfo], reg: u32, bit: u8) -> bool {
    for s in l.iter().take(WINDOW * 4) {
        if !stmt_reads_comp(s, reads, reg, bit).is_empty() {
            return false;
        }
        match s {
            Stmt::Assign { reg: r2, mask: m2, .. } => {
                if *r2 == reg && m2 & bit != 0 {
                    return true;
                }
            }
            Stmt::Eff(_) | Stmt::Comment(_) => {}
            _ => return false,
        }
    }
    false
}

/// Registers/components that the plain statements at the start of `l` overwrite before anything reads them.
fn killed_map(l: &[Stmt], reads: &[ReadInfo]) -> HashMap<u32, u8> {
    let mut killed: HashMap<u32, u8> = HashMap::new();
    let mut seen_read: HashMap<u32, u8> = HashMap::new();
    for s in l.iter().take(WINDOW * 4) {
        if !matches!(s, Stmt::Assign { .. } | Stmt::Eff(_) | Stmt::Comment(_)) {
            break;
        }
        heads(s, &mut |e| {
            walk_reads(e, &mut |id| {
                if let Some(r) = reads.get(id) {
                    let m = r.comps.iter().fold(0u8, |a, &c| a | (1 << c));
                    *seen_read.entry(r.reg).or_default() |= m;
                }
            })
        });
        if let Stmt::Assign { reg, mask, .. } = s {
            let r = seen_read.get(reg).copied().unwrap_or(0);
            *killed.entry(*reg).or_default() |= mask & !r;
        }
    }
    killed
}

/// Fold single-use temporaries into their reader. A definition `D: rN.M = E` moves into the reader `U` when
/// - `D` and `U` are in the same statement list, only plain assignments/effects lie between them, none of which writes a
///   register `E` reads or a component of `M`, and `U` is the first statement reading any component of `M` through exactly one
///   marker that is read nowhere else;
/// - for every component of `M` that is also read somewhere else in the shader, a later plain statement of the same list
///   overwrites it before any further read (so no other reader, in particular none reached by a loop back edge or a jump out
///   of the block, can observe `D`'s value). If `U` itself is a control statement, no other reader may exist at all.
fn fold_list(list: &mut Vec<Stmt>, reads: &[ReadInfo], fi: &FoldInfo, in_loop: bool, brk: Option<&HashMap<u32, u8>>) {
    // nested lists first; a loop body learns which registers are overwritten right after the loop (the target of `break`)
    for k in 0..list.len() {
        let after = killed_map(&list[k + 1..], reads);
        match &mut list[k] {
            Stmt::If { then, els, .. } => {
                fold_list(then, reads, fi, in_loop, brk);
                fold_list(els, reads, fi, in_loop, brk);
            }
            Stmt::Loop(b) => fold_list(b, reads, fi, true, Some(&after)),
            Stmt::Switch { body: b, .. } => fold_list(b, reads, fi, in_loop, None),
            _ => {}
        }
    }
    let mut i = 0;
    while i < list.len() {
        let cand = match &list[i] {
            Stmt::Assign { reg, mask, e, pure: true, rid1 } => Some((*reg, *mask, e, *rid1)),
            _ => None,
        };
        let Some((reg, mask, e, rid1)) = cand else {
            i += 1;
            continue;
        };
        let mut dep: Vec<(u32, u8)> = Vec::new();
        walk_reads(e, &mut |rid| {
            if let Some(r) = reads.get(rid) {
                for &c in &r.comps {
                    dep.push((r.reg, c));
                }
            }
        });
        let end = list.len().min(i + 1 + WINDOW);
        // first statement that reads a component of M
        let mut target = None;
        for k in i + 1..end {
            let hits = stmt_reads_comp(&list[k], reads, reg, mask);
            if !hits.is_empty() {
                let id = hits[0];
                if hits.len() == 1 && fi.occurrences.get(&id).copied().unwrap_or(0) == 1 && reads[id].comps.iter().all(|&c| mask & (1 << c) != 0) {
                    target = Some((k, id));
                }
                break;
            }
            match &list[k] {
                Stmt::Assign { reg: r2, mask: m2, .. } => {
                    if (*r2 == reg && m2 & mask != 0) || dep.iter().any(|&(r, c)| r == *r2 && m2 & (1 << c) != 0) {
                        break;
                    }
                }
                Stmt::Eff(_) | Stmt::Comment(_) => {}
                _ => break,
            }
        }
        let Some((j, id)) = target else {
            i += 1;
            continue;
        };
        let mut ok = true;
        for c in 0..4u8 {
            if mask & (1 << c) == 0 {
                continue;
            }
            // readers created before this definition's instruction cannot observe it (outside loops, where the back edge could)
            let relevant = |x: usize| x != id && (in_loop || x >= rid1);
            let Some(others) = fi.reads_by_comp.get(&(reg, c)).map(|v| v.iter().copied().filter(|&x| relevant(x)).collect::<Vec<_>>()) else {
                continue;
            };
            if others.is_empty() {
                continue;
            }
            let bit = 1u8 << c;
            let rest = &list[j + 1..];
            let self_kill = matches!(&list[j], Stmt::Assign { reg: r2, mask: m2, .. } if *r2 == reg && m2 & bit != 0);
            let good = match &list[j] {
                Stmt::Assign { .. } | Stmt::Eff(_) => self_kill || killed(rest, reads, reg, bit),
                Stmt::Jump { kind: "return" | "discard", .. } => killed(rest, reads, reg, bit),
                Stmt::Jump { kind: "break", .. } => killed(rest, reads, reg, bit) && brk.map(|m| m.get(&reg).map(|k| k & bit != 0).unwrap_or(false)).unwrap_or(false),
                Stmt::If { then, els, .. } => {
                    let after = killed(rest, reads, reg, bit);
                    (after || killed(then, reads, reg, bit)) && (after || killed(els, reads, reg, bit))
                }
                _ => false,
            };
            if !good {
                ok = false;
                break;
            }
        }
        if !ok {
            i += 1;
            continue;
        }
        let info = &reads[id];
        let Stmt::Assign { e, .. } = list.remove(i) else { unreachable!() };
        let pos: Vec<u8> = info
            .comps
            .iter()
            .map(|&c| if e.w <= 1 { 0 } else { (0..4u8).filter(|k| mask & (1 << k) != 0).position(|k| k == c).unwrap_or(0) as u8 })
            .collect();
        let rep = coerce(swz(e, &pos), info.want);
        let mut with = Some(rep);
        heads_mut(&mut list[j - 1], &mut |x| replace_read(x, id, &mut with));
        // do not advance: the statement now at index i is the next candidate
    }
}

fn comps_of_mask(mask: u8) -> Vec<u8> {
    (0..4u8).filter(|c| mask & (1 << c) != 0).collect()
}

struct Out<'a> {
    pr: Printer<'a>,
    s: String,
    ret_void: bool,
    ty_of_reg: &'a [Ty],
}

impl<'a> Out<'a> {
    fn line(&mut self, depth: usize, t: &str) {
        for _ in 0..depth {
            self.s.push_str("    ");
        }
        self.s.push_str(t);
        self.s.push('\n');
    }

    fn cond(&self, e: &Ex, nz: bool) -> String {
        if nz {
            self.pr.bool_of(e).0
        } else {
            self.pr.not_of(e).0
        }
    }

    fn list(&mut self, l: &[Stmt], depth: usize) {
        for st in l {
            self.stmt(st, depth);
        }
    }

    fn stmt(&mut self, st: &Stmt, depth: usize) {
        match st {
            Stmt::Assign { reg, mask, e, .. } => {
                let ty = self.ty_of_reg.get(*reg as usize).copied().unwrap_or(Ty::F);
                let rhs = coerce(e.clone(), ty);
                let lhs = if *mask == 0xf { format!("r{}", reg) } else { format!("r{}.{}", reg, letters(&comps_of_mask(*mask))) };
                let t = format!("{} = {};", lhs, self.pr.min(&rhs, 0));
                self.line(depth, &t);
            }
            Stmt::Eff(e) => {
                let t = format!("{};", self.pr.min(e, 0));
                self.line(depth, &t);
            }
            Stmt::If { cond, nz, then, els } => {
                let c = self.cond(cond, *nz);
                self.line(depth, &format!("if ({}) {{", c));
                self.list(then, depth + 1);
                if !els.is_empty() {
                    self.line(depth, "} else {");
                    self.list(els, depth + 1);
                }
                self.line(depth, "}");
            }
            Stmt::Loop(b) => {
                self.line(depth, "while (true) {");
                self.list(b, depth + 1);
                self.line(depth, "}");
            }
            Stmt::Switch { sel, body } => {
                let t = format!("switch ({}) {{", self.pr.min(sel, 0));
                self.line(depth, &t);
                for s in body {
                    match s {
                        Stmt::Label(l) => self.line(depth, l),
                        other => self.stmt(other, depth + 1),
                    }
                }
                self.line(depth, "}");
            }
            Stmt::Label(l) => self.line(depth, l),
            Stmt::Jump { kind, cond } => {
                let body = match *kind {
                    "return" => {
                        if self.ret_void {
                            "return;".to_string()
                        } else {
                            "return o;".to_string()
                        }
                    }
                    k => format!("{};", k),
                };
                match cond {
                    None => self.line(depth, &body),
                    Some((c, nz)) => {
                        let cs = self.cond(c, *nz);
                        self.line(depth, &format!("if ({}) {}", cs, body));
                    }
                }
            }
            Stmt::Comment(c) => {
                for l in c.lines() {
                    self.line(depth, &format!("// {}", l));
                }
            }
        }
    }
}

fn used_regs(list: &[Stmt], reads: &[ReadInfo]) -> BTreeSet<u32> {
    let mut set = BTreeSet::new();
    walk_stmts(list, &mut |s| {
        heads(s, &mut |e| walk_reads(e, &mut |id| {
            if let Some(r) = reads.get(id) {
                set.insert(r.reg);
            }
        }));
        if let Stmt::Assign { reg, .. } = s {
            set.insert(*reg);
        }
    });
    set
}

fn strip_trailing_return(l: &mut Vec<Stmt>) {
    if matches!(l.last(), Some(Stmt::Jump { kind: "return", cond: None })) {
        l.pop();
    }
}

fn struct_decl(name: &str, sig: &[SigElem], interp: Option<&HashMap<u32, u32>>, regname: &str) -> String {
    let mut s = format!("struct {} {{\n", name);
    let mut seen = BTreeSet::new();
    for e in sig {
        let f = sig_field(e);
        if !seen.insert(f.name.clone()) {
            continue;
        }
        let ty = f.ty.vec(f.width);
        let sem = if e.name.starts_with("SV_") { e.name.clone() } else { e.name.clone() };
        let semi = if e.index == 0 && e.name.starts_with("SV_") { sem } else { format!("{}{}", sem, e.index) };
        let modifier = interp
            .and_then(|m| m.get(&e.reg))
            .map(|m| match m {
                1 => "nointerpolation ",
                3 => "centroid ",
                4 => "noperspective ",
                5 => "noperspective centroid ",
                6 | 7 => "sample ",
                _ => "",
            })
            .unwrap_or("");
        s.push_str(&format!("    {}{} {} : {}; // {}{}.{}\n", modifier, ty, f.name, semi, regname, if e.reg == u32::MAX { 0 } else { e.reg }, letters(&comps_of_mask(e.mask))));
    }
    s.push_str("};\n");
    s
}

fn collect_structs(t: &RType, seen: &mut BTreeSet<String>, out: &mut String) {
    for (_, m, _) in &t.members {
        collect_structs(m, seen, out);
    }
    if t.class == 5 && !t.name.is_empty() && seen.insert(t.name.clone()) {
        out.push_str(&format!("struct {} {{\n", t.name));
        for (n, m, off) in &t.members {
            out.push_str(&format!("    {} {}{}; // offset {}\n", type_decl(m), n, if m.elements > 0 { format!("[{}]", m.elements) } else { String::new() }, off));
        }
        out.push_str("};\n\n");
    }
}

fn resource_decls(tr: &Tr) -> String {
    let mut s = String::new();
    let p = tr.p;
    let mut seen: BTreeSet<(u8, u32)> = BTreeSet::new();
    for i in &p.instrs {
        let Some(n) = i.ops.first().and_then(|o| o.index(0)) else { continue };
        match i.op {
            88 | 161 | 162 => {
                if !seen.insert((0, n)) {
                    continue;
                }
                let (d, rt) = match i.op {
                    88 => (i.control & 31, i.extra.first().copied().unwrap_or(0)),
                    161 => (11, 0),
                    _ => (12, 0),
                };
                let name = tr.tex_name(n);
                let tyname = tex_type_name(d);
                let ms = if d == 4 || d == 9 { format!(", {}", (i.control >> 5) & 0x7f) } else { String::new() };
                let decl = match d {
                    11 => format!("ByteAddressBuffer {}", name),
                    12 => format!("StructuredBuffer<uint> {} /* stride {} bytes */", name, i.extra.first().copied().unwrap_or(0)),
                    _ => {
                        let rn = ret_ty(rt);
                        let comps = (0..4).filter(|k| (rt >> (4 * k)) & 15 != 0 && (rt >> (4 * k)) & 15 != 9).count().max(1);
                        let prefix = match rt & 15 {
                            1 => "unorm ",
                            2 => "snorm ",
                            _ => "",
                        };
                        format!("{}<{}{}{}> {}", tyname, prefix, rn.vec(comps as u8).replace("uint", "uint"), ms, name)
                    }
                };
                s.push_str(&format!("{} : register(t{});\n", decl, n));
            }
            90 => {
                if seen.insert((1, n)) {
                    let k = if tr.smp_cmp.contains(&n) { "SamplerComparisonState" } else { "SamplerState" };
                    s.push_str(&format!("{} {} : register(s{});\n", k, tr.smp_name(n), n));
                }
            }
            156 | 157 | 158 => {
                if seen.insert((2, n)) {
                    let name = tr.uav_name(n);
                    let decl = match i.op {
                        156 => {
                            let d = i.control & 31;
                            let rt = i.extra.first().copied().unwrap_or(0);
                            format!("RW{}<{}> {}", tex_type_name(d), ret_ty(rt).vec(4), name)
                        }
                        157 => format!("RWByteAddressBuffer {}", name),
                        _ => format!("RWStructuredBuffer<uint> {} /* stride {} bytes */", name, i.extra.first().copied().unwrap_or(0)),
                    };
                    s.push_str(&format!("{} : register(u{});\n", decl, n));
                }
            }
            159 => s.push_str(&format!("groupshared uint g{}[{}]; // raw, {} bytes\n", n, i.extra.first().copied().unwrap_or(0) / 4, i.extra.first().copied().unwrap_or(0))),
            160 => s.push_str(&format!(
                "groupshared uint g{}[{}]; // structured: {} elements of {} bytes\n",
                n,
                i.extra.first().copied().unwrap_or(0) * i.extra.get(1).copied().unwrap_or(0) / 4,
                i.extra.get(1).copied().unwrap_or(0),
                i.extra.first().copied().unwrap_or(0)
            )),
            _ => {}
        }
    }
    s
}

fn cbuffer_decls(tr: &Tr) -> String {
    let mut s = String::new();
    let p = tr.p;
    let mut declared = BTreeSet::new();
    let mut structs = String::new();
    let mut seen_structs = BTreeSet::new();
    let mut body = String::new();
    for i in &p.instrs {
        if i.op != 89 {
            continue;
        }
        let Some(slot) = i.ops.first().and_then(|o| o.index(0)) else { continue };
        let size = i.ops.first().and_then(|o| o.index(1)).unwrap_or(0);
        if !declared.insert(slot) {
            continue;
        }
        match tr.cb_for_slot(slot) {
            Some(cb) => {
                let any_used = cb.vars.iter().any(|v| v.flags & 2 != 0);
                body.push_str(&format!("cbuffer {} : register(b{}) // {} bytes{}\n{{\n", cb.name, slot, cb.size, if i.control & 1 != 0 { ", dynamically indexed" } else { "" }));
                for v in &cb.vars {
                    if any_used && v.flags & 2 == 0 && !tr.cb_used.contains(&(slot, v.name.clone())) {
                        continue;
                    }
                    collect_structs(&v.ty, &mut seen_structs, &mut structs);
                    body.push_str(&format!(
                        "    {} {}{}; // offset {}\n",
                        type_decl(&v.ty),
                        v.name,
                        if v.ty.elements > 0 { format!("[{}]", v.ty.elements) } else { String::new() },
                        v.offset
                    ));
                }
                body.push_str("};\n\n");
            }
            None => {
                body.push_str(&format!("float4 cb{}[{}]; // register(b{}), no reflection data\n\n", slot, if size == 0 { 4096 } else { size }, slot));
            }
        }
    }
    s.push_str(&structs);
    s.push_str(&body);
    for slot in &tr.raw_cbs {
        if let Some(cb) = tr.cb_for_slot(*slot) {
            s.push_str(&format!("// cb{}[n] below means register n of cbuffer {} (16 bytes each)\n", slot, cb.name));
        } else {
            s.push_str(&format!("// cb{}[n] below means register n of constant buffer slot {}\n", slot, slot));
        }
    }
    s
}

fn run_pass<'a>(p: &'a Program, sections: &[(u8, Vec<&Instr>)], regty: Vec<Ty>) -> (Tr<'a>, Vec<(String, u8, Vec<Stmt>)>, Vec<Vec<ReadInfo>>) {
    let mut tr = Tr::new(p, regty);
    let mut bodies: Vec<(String, u8, Vec<Stmt>)> = Vec::new();
    let mut read_tables: Vec<Vec<ReadInfo>> = Vec::new();
    for (ph, ins) in sections {
        tr.phase = *ph;
        tr.reads.clear();
        let mut b = tr.translate(ins);
        strip_trailing_return(&mut b);
        let fi = analyse(&b, &tr.reads);
        if !p.instrs.iter().any(|i| matches!(i.op, 4 | 5 | 44)) {
            fold_list(&mut b, &tr.reads, &fi, false, None);
        }
        read_tables.push(std::mem::take(&mut tr.reads));
        let name = match (p.stage, ph) {
            (3, 0) => "hs_control_point_phase",
            (3, 1) => "hs_fork_phase",
            (3, 2) => "hs_join_phase",
            _ => "main",
        };
        bodies.push((name.to_string(), *ph, b));
    }
    (tr, bodies, read_tables)
}

fn votes_of(bodies: &[(String, u8, Vec<Stmt>)]) -> Vec<Ty> {
    let mut votes: Vec<[u32; 3]> = Vec::new();
    for (_, _, b) in bodies {
        walk_stmts(b, &mut |s| {
            if let Stmt::Assign { reg, mask, e, .. } = s {
                let k = match e.ty {
                    Ty::F => 0,
                    Ty::I => 1,
                    Ty::U => 2,
                    Ty::Any => return,
                };
                if votes.len() <= *reg as usize {
                    votes.resize(*reg as usize + 1, [0; 3]);
                }
                votes[*reg as usize][k] += mask.count_ones();
            }
        });
    }
    votes
        .iter()
        .map(|v| {
            if v[1] > v[0] && v[1] >= v[2] {
                Ty::I
            } else if v[2] > v[0] && v[2] > v[1] {
                Ty::U
            } else {
                Ty::F
            }
        })
        .collect()
}

/// Decompile `p` to HLSL-like text. Warnings (unsupported constructs) are appended to `warnings`.
pub(crate) fn decompile(p: &Program, warnings: &mut Vec<String>) -> String {
    let sections = split_phases(p);
    // Register types: translate, fold, vote on the surviving writes, repeat (the first round assumes float everywhere).
    let mut regty: Vec<Ty> = Vec::new();
    let (mut tr, mut bodies, mut read_tables) = run_pass(p, &sections, regty.clone());
    for _ in 0..2 {
        regty = votes_of(&bodies);
        (tr, bodies, read_tables) = run_pass(p, &sections, regty.clone());
    }
    warnings.extend(tr.warnings.iter().cloned());

    let stage = super::stage_name(p.stage);
    let mut s = String::new();
    s.push_str(&format!("// {}_{}_{} decompiled to HLSL-like code (a readable rendering, not the original source)\n", stage, p.major, p.minor));
    s.push_str("// rN = temporaries; asfloat/asint/asuint are bit casts of the 32-bit register contents.\n\n");
    s.push_str(&cbuffer_decls(&tr));
    s.push_str(&resource_decls(&tr));
    if tr.uses_icb {
        if let Some(d) = tr.icb_data() {
            let fl = tr.icb_is_float();
            let n = d.len() / 4;
            s.push_str(&format!("static const {} icb[{}] = {{\n", if fl { "float4" } else { "uint4" }, n));
            for c in d.chunks(4) {
                let items: Vec<String> = c
                    .iter()
                    .map(|&v| if fl { lit_text(v, Ty::F) } else { lit_text(v, Ty::U) })
                    .collect();
                s.push_str(&format!("    {}({}),\n", if fl { "float4" } else { "uint4" }, items.join(", ")));
            }
            s.push_str("};\n");
        }
    }
    let mut ind: Vec<(u32, u32, u32)> = Vec::new();
    for i in &p.instrs {
        if i.op == 105 {
            ind.push((i.extra.first().copied().unwrap_or(0), i.extra.get(1).copied().unwrap_or(0), i.extra.get(2).copied().unwrap_or(4)));
        }
    }
    for (n, sz, _c) in &ind {
        s.push_str(&format!("static float4 x{}[{}];\n", n, sz));
    }
    s.push('\n');

    let isg_name = format!("{}_IN", stage.to_uppercase());
    let osg_name = format!("{}_OUT", stage.to_uppercase());
    if !p.isg.is_empty() && p.stage != 5 {
        s.push_str(&struct_decl(&isg_name, &p.isg, if is_ps(p) { Some(&tr.ps_interp) } else { None }, "v"));
        s.push('\n');
    }
    if !p.osg.is_empty() && p.stage != 5 {
        s.push_str(&struct_decl(&osg_name, &p.osg, None, "o"));
        s.push('\n');
    }
    if !p.psg.is_empty() {
        s.push_str(&struct_decl("PATCH", &p.psg, None, "o"));
        s.push('\n');
    }

    let void = p.stage == 5 || p.stage == 3 && false;
    for (k, (name, ph, mut body)) in bodies.into_iter().enumerate() {
        let reads = &read_tables[k];
        let mut params: Vec<String> = Vec::new();
        let has_in = !p.isg.is_empty() && p.stage != 5;
        match p.stage {
            2 => {
                let prim = p.instrs.iter().find(|i| i.op == 93).map(|i| i.control & 0x7f).unwrap_or(3);
                let (kw, n) = match prim {
                    1 => ("point", 1),
                    2 => ("line", 2),
                    3 => ("triangle", 3),
                    6 => ("lineadj", 4),
                    7 => ("triangleadj", 6),
                    _ => ("triangle", 3),
                };
                params.push(format!("{} {} i[{}]", kw, isg_name, n));
                let topo = p.instrs.iter().find(|i| i.op == 92).map(|i| i.control & 0x7f).unwrap_or(5);
                let st = match topo {
                    1 => "PointStream",
                    3 => "LineStream",
                    _ => "TriangleStream",
                };
                params.push(format!("inout {}<{}> stream", st, osg_name));
            }
            3 | 4 if has_in => {
                let cp = p.instrs.iter().find(|i| i.op == 147).map(|i| i.control & 0x3f).unwrap_or(1);
                params.push(format!("InputPatch<{}, {}> i", isg_name, cp));
                if p.stage == 4 && !p.psg.is_empty() {
                    params.push("PATCH pc".into());
                }
            }
            _ => {
                if has_in {
                    params.push(format!("{} i", isg_name));
                }
            }
        }
        for b in tr.builtins.values() {
            params.push(b.clone());
        }
        let ret = if p.stage == 5 || p.stage == 2 || (p.stage == 3 && ph >= 1) || p.osg.is_empty() {
            "void".to_string()
        } else if p.stage == 3 {
            "void".into()
        } else {
            osg_name.clone()
        };
        let ret_void = ret == "void";
        let _ = void;
        // attributes
        match p.stage {
            5 => {
                if let Some(tg) = p.instrs.iter().find(|i| i.op == 155) {
                    s.push_str(&format!("[numthreads({}, {}, {})]\n", tg.extra.first().copied().unwrap_or(1), tg.extra.get(1).copied().unwrap_or(1), tg.extra.get(2).copied().unwrap_or(1)));
                }
            }
            2 => {
                if let Some(m) = p.instrs.iter().find(|i| i.op == 94) {
                    s.push_str(&format!("[maxvertexcount({})]\n", m.extra.first().copied().unwrap_or(0)));
                }
            }
            3 if ph == 0 => {
                let dom = p.instrs.iter().find(|i| i.op == 149).map(|i| i.control & 7).unwrap_or(0);
                let part = p.instrs.iter().find(|i| i.op == 150).map(|i| i.control & 7).unwrap_or(0);
                let outp = p.instrs.iter().find(|i| i.op == 151).map(|i| i.control & 7).unwrap_or(0);
                let ocp = p.instrs.iter().find(|i| i.op == 148).map(|i| i.control & 0x3f).unwrap_or(0);
                s.push_str(&format!(
                    "// domain({}) partitioning({}) outputtopology({}) outputcontrolpoints({})\n",
                    ["?", "isoline", "tri", "quad"][dom as usize % 4],
                    ["?", "integer", "pow2", "fractional_odd", "fractional_even"][part as usize % 5],
                    ["?", "point", "line", "triangle_cw", "triangle_ccw"][outp as usize % 5],
                    ocp
                ));
            }
            3 => {
                if let Some(c) = p.instrs.iter().find(|i| i.op == if ph == 1 { 153 } else { 154 }) {
                    s.push_str(&format!("// instance count {}\n", c.extra.first().copied().unwrap_or(1)));
                }
            }
            _ => {}
        }
        s.push_str(&format!("{} {}({}) {{\n", ret, name, params.join(", ")));
        if !ret_void {
            s.push_str(&format!("    {} o = ({})0;\n", osg_name, osg_name));
        } else if p.stage == 3 && ph >= 1 && !p.psg.is_empty() {
            s.push_str("    PATCH o = (PATCH)0; // patch constant outputs\n");
        } else if p.stage == 3 && ph == 0 && !p.osg.is_empty() {
            s.push_str(&format!("    {} o[/*output control points*/ 32]; // control point outputs\n", osg_name));
        } else if p.stage == 2 {
            s.push_str(&format!("    {} o = ({})0;\n", osg_name, osg_name));
        }
        let used = used_regs(&body, reads);
        let mut by_ty: BTreeMap<u8, Vec<String>> = BTreeMap::new();
        for r in &used {
            let t = regty.get(*r as usize).copied().unwrap_or(Ty::F);
            by_ty.entry(t as u8).or_default().push(format!("r{}", r));
        }
        for (t, names) in by_ty {
            let ty = match t {
                0 => Ty::F,
                1 => Ty::I,
                _ => Ty::U,
            };
            s.push_str(&format!("    {} {};\n", ty.vec(4), names.join(", ")));
        }
        let ctx = RCtx { reads, regty: &regty };
        let mut o = Out { pr: Printer { c: &ctx }, s: String::new(), ret_void, ty_of_reg: &regty };
        o.list(&body, 1);
        s.push_str(&o.s);
        if !ret_void {
            s.push_str("    return o;\n");
        }
        s.push_str("}\n\n");
        body.clear();
    }
    s
}

fn lit_text(v: u32, ty: Ty) -> String {
    let reads: [ReadInfo; 0] = [];
    let regty: [Ty; 0] = [];
    let c = RCtx { reads: &reads, regty: &regty };
    Printer { c: &c }.render(&lit(vec![v], ty)).0
}

/// Split the program into phases (hull shaders have a control point phase and fork/join phases); other stages are one section.
fn split_phases(p: &Program) -> Vec<(u8, Vec<&Instr>)> {
    if p.stage != 3 {
        return vec![(0, p.instrs.iter().collect())];
    }
    let mut v: Vec<(u8, Vec<&Instr>)> = Vec::new();
    let mut started = false;
    for i in &p.instrs {
        match i.op {
            114 => {
                v.push((0, Vec::new()));
                started = true;
            }
            115 => {
                v.push((1, Vec::new()));
                started = true;
            }
            116 => {
                v.push((2, Vec::new()));
                started = true;
            }
            _ => {
                if started {
                    if let Some(l) = v.last_mut() {
                        l.1.push(i);
                    }
                }
            }
        }
    }
    if v.is_empty() {
        v.push((0, p.instrs.iter().collect()));
    }
    v
}

#[allow(dead_code)]
fn unused(_: u32) -> &'static str {
    let _ = (dim_name, sysval_name);
    ""
}
