//! Token-level decoding of the SM4/SM5 shader byte code (SHEX / SHDR chunk body) into instructions and operands.
//!
//! Layout (the public `d3d11tokenizedprogramformat.h` token layout; EMPIRICAL on the game's shader cache, where the whole
//! corpus decodes with consistent instruction lengths): opcode token = opcode (bits 0-10), control (bits 11-23), length in
//! dwords (bits 24-30), extended flag (bit 31); operand token = component count (0-1), selection mode (2-3),
//! mask/swizzle/select (4-11), operand type (12-19), index dimension (20-21), three 3-bit index representations (22-30),
//! extended flag (31).

/// Operand selection (mask for destinations, swizzle for sources, single component).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sel {
    None,
    Mask(u8),
    Swz([u8; 4]),
    One(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Idx {
    Imm(u32),
    /// `imm + register` (imm is 0 for pure relative).
    Rel(u32, Box<Operand>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Operand {
    pub ty: u8,
    pub ncomp: u8,
    pub sel: Sel,
    pub idx: Vec<Idx>,
    /// Immediate payload (operand type 4: 1 or 4 dwords, type 5: 2 or 8 dwords).
    pub imm: Vec<u32>,
    pub neg: bool,
    pub abs: bool,
    pub min_prec: u8,
    pub nonuniform: bool,
}

impl Operand {
    pub fn index(&self, i: usize) -> Option<u32> {
        match self.idx.get(i) {
            Some(Idx::Imm(v)) => Some(*v),
            _ => None,
        }
    }
    /// Source swizzle as four component indices (select1 replicates; masks are widened in order).
    pub fn swizzle(&self) -> [u8; 4] {
        match self.sel {
            Sel::Swz(s) => s,
            Sel::One(c) => [c; 4],
            Sel::Mask(m) => {
                let mut o = [0u8; 4];
                let mut k = 0;
                for c in 0..4 {
                    if m & (1 << c) != 0 {
                        o[k] = c as u8;
                        k += 1;
                    }
                }
                for i in k..4 {
                    o[i] = if k > 0 { o[k - 1] } else { 0 };
                }
                o
            }
            Sel::None => [0, 1, 2, 3],
        }
    }
    /// Destination write mask as a bit set (xyzw = 1,2,4,8). Scalar/none operands count as `x`.
    pub fn mask(&self) -> u8 {
        match self.sel {
            Sel::Mask(m) => m,
            Sel::One(c) => 1 << (c & 3),
            Sel::Swz(_) => 0xf,
            Sel::None => 1,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Instr {
    pub op: u16,
    /// Raw control bits 11..23 shifted down to bit 0.
    pub control: u32,
    pub sat: bool,
    pub test_nz: bool,
    pub precise: u8,
    pub offset: [i8; 3],
    pub has_offset: bool,
    pub ext_dim: Option<(u8, u16)>,
    pub ext_ret: Option<[u8; 4]>,
    pub ops: Vec<Operand>,
    /// Trailing literal dwords of declarations (return types, names, counts, strides ...), or the raw rest when unparsable.
    pub extra: Vec<u32>,
    /// Custom data (class, payload dwords).
    pub custom: Option<(u32, Vec<u32>)>,
    pub bad: bool,
    pub unknown: bool,
}

pub const OP_CUSTOMDATA: u16 = 53;

const NAMES: &[&str] = &[
    "add", "and", "break", "breakc", "call", "callc", "case", "continue", "continuec", "cut", "default", "deriv_rtx", "deriv_rty",
    "discard", "div", "dp2", "dp3", "dp4", "else", "emit", "emitThenCut", "endif", "endloop", "endswitch", "eq", "exp", "frc", "ftoi",
    "ftou", "ge", "iadd", "if", "ieq", "ige", "ilt", "imad", "imax", "imin", "imul", "ine", "ineg", "ishl", "ishr", "itof", "label",
    "ld", "ld_ms", "log", "loop", "lt", "mad", "min", "max", "customdata", "mov", "movc", "mul", "ne", "nop", "not", "or", "resinfo",
    "ret", "retc", "round_ne", "round_ni", "round_pi", "round_z", "rsq", "sample", "sample_c", "sample_c_lz", "sample_l", "sample_d",
    "sample_b", "sqrt", "switch", "sincos", "udiv", "ult", "uge", "umul", "umad", "umax", "umin", "ushr", "utof", "xor",
    "dcl_resource", "dcl_constantbuffer", "dcl_sampler", "dcl_index_range", "dcl_outputtopology", "dcl_inputprimitive", "dcl_maxout",
    "dcl_input", "dcl_input_sgv", "dcl_input_siv", "dcl_input_ps", "dcl_input_ps_sgv", "dcl_input_ps_siv", "dcl_output",
    "dcl_output_sgv", "dcl_output_siv", "dcl_temps", "dcl_indexableTemp", "dcl_globalFlags", "reserved0", "lod", "gather4",
    "samplepos", "sampleinfo", "reserved1", "hs_decls", "hs_control_point_phase", "hs_fork_phase", "hs_join_phase", "emit_stream",
    "cut_stream", "emitThenCut_stream", "interface_call", "bufinfo", "deriv_rtx_coarse", "deriv_rtx_fine", "deriv_rty_coarse",
    "deriv_rty_fine", "gather4_c", "gather4_po", "gather4_po_c", "rcp", "f32tof16", "f16tof32", "uaddc", "usubb", "countbits",
    "firstbit_hi", "firstbit_lo", "firstbit_shi", "ubfe", "ibfe", "bfi", "bfrev", "swapc", "dcl_stream", "dcl_function_body",
    "dcl_function_table", "dcl_interface", "dcl_input_control_point_count", "dcl_output_control_point_count",
    "dcl_tessellator_domain", "dcl_tessellator_partitioning", "dcl_tessellator_output_primitive", "dcl_hs_max_tessfactor",
    "dcl_hs_fork_phase_instance_count", "dcl_hs_join_phase_instance_count", "dcl_thread_group", "dcl_uav_typed", "dcl_uav_raw",
    "dcl_uav_structured", "dcl_tgsm_raw", "dcl_tgsm_structured", "dcl_resource_raw", "dcl_resource_structured", "ld_uav_typed",
    "store_uav_typed", "ld_raw", "store_raw", "ld_structured", "store_structured", "atomic_and", "atomic_or", "atomic_xor",
    "atomic_cmp_store", "atomic_iadd", "atomic_imax", "atomic_imin", "atomic_umax", "atomic_umin", "imm_atomic_alloc",
    "imm_atomic_consume", "imm_atomic_iadd", "imm_atomic_and", "imm_atomic_or", "imm_atomic_xor", "imm_atomic_exch",
    "imm_atomic_cmp_exch", "imm_atomic_imax", "imm_atomic_imin", "imm_atomic_umax", "imm_atomic_umin", "sync", "dadd", "dmax",
    "dmin", "dmul", "deq", "dge", "dlt", "dne", "dmov", "dmovc", "dtof", "ftod", "eval_snapped", "eval_sample_index",
    "eval_centroid", "dcl_gs_instance_count", "abort", "debug_break", "ddiv", "dfma", "drcp", "msad", "dtoi", "dtou", "itod", "utod",
];

pub fn opname(op: u16) -> Option<&'static str> {
    NAMES.get(op as usize).copied()
}

/// Declarations and phase markers (no saturate bit, control bits mean something else).
pub fn is_decl(op: u16) -> bool {
    matches!(op, 88..=106 | 113..=116 | 143..=162 | 206)
}

fn parse_operand(toks: &[u32], pos: &mut usize, depth: u32) -> Result<Operand, String> {
    if depth > 4 {
        return Err("operand nesting too deep".into());
    }
    let t = *toks.get(*pos).ok_or("truncated operand")?;
    *pos += 1;
    let ncomp = (t & 3) as u8;
    let mode = (t >> 2) & 3;
    let ty = ((t >> 12) & 0xff) as u8;
    let dim = ((t >> 20) & 3) as usize;
    let reps = [(t >> 22) & 7, (t >> 25) & 7, (t >> 28) & 7];
    let sel = if ncomp == 2 && ty != 4 && ty != 5 {
        match mode {
            0 => Sel::Mask(((t >> 4) & 0xf) as u8),
            1 => Sel::Swz([((t >> 4) & 3) as u8, ((t >> 6) & 3) as u8, ((t >> 8) & 3) as u8, ((t >> 10) & 3) as u8]),
            2 => Sel::One(((t >> 4) & 3) as u8),
            _ => Sel::None,
        }
    } else {
        Sel::None
    };
    let mut o = Operand { ty, ncomp, sel, idx: vec![], imm: vec![], neg: false, abs: false, min_prec: 0, nonuniform: false };
    let mut ext = t & 0x8000_0000 != 0;
    let mut guard = 0;
    while ext {
        let e = *toks.get(*pos).ok_or("truncated operand extension")?;
        *pos += 1;
        if e & 0x3f == 1 {
            let m = (e >> 6) & 0xff;
            o.neg = m & 1 != 0;
            o.abs = m & 2 != 0;
            o.min_prec = ((e >> 14) & 7) as u8;
            o.nonuniform = (e >> 17) & 1 != 0;
        }
        ext = e & 0x8000_0000 != 0;
        guard += 1;
        if guard > 4 {
            return Err("operand extension chain too long".into());
        }
    }
    if ty == 4 || ty == 5 {
        let n = match (ty, ncomp) {
            (4, 1) => 1,
            (4, 2) => 4,
            (5, 1) => 2,
            (5, 2) => 8,
            _ => return Err(format!("immediate operand with {} components", ncomp)),
        };
        if *pos + n > toks.len() {
            return Err("truncated immediate".into());
        }
        o.imm = toks[*pos..*pos + n].to_vec();
        *pos += n;
    }
    for rep in reps.iter().take(dim) {
        let next = |pos: &mut usize| -> Result<u32, String> {
            let v = *toks.get(*pos).ok_or("truncated operand index")?;
            *pos += 1;
            Ok(v)
        };
        match *rep {
            0 => o.idx.push(Idx::Imm(next(pos)?)),
            1 => {
                let lo = next(pos)?;
                let _hi = next(pos)?;
                o.idx.push(Idx::Imm(lo));
            }
            2 => {
                let r = parse_operand(toks, pos, depth + 1)?;
                o.idx.push(Idx::Rel(0, Box::new(r)));
            }
            3 => {
                let v = next(pos)?;
                let r = parse_operand(toks, pos, depth + 1)?;
                o.idx.push(Idx::Rel(v, Box::new(r)));
            }
            4 => {
                let v = next(pos)?;
                let _hi = next(pos)?;
                let r = parse_operand(toks, pos, depth + 1)?;
                o.idx.push(Idx::Rel(v, Box::new(r)));
            }
            r => return Err(format!("unknown index representation {}", r)),
        }
    }
    Ok(o)
}

fn sext4(v: u32) -> i8 {
    (((v & 0xf) as i8) << 4) >> 4
}

/// Declaration shape: (operand count, literal dwords after the operands). None = not a declaration with a fixed shape.
fn decl_shape(op: u16) -> Option<(usize, usize)> {
    Some(match op {
        88 => (1, 1),
        89 | 90 | 95 | 98 | 101 | 143 | 144 | 157 | 161 => (1, 0),
        91 | 96 | 97 | 99 | 100 | 102 | 103 | 156 | 158 | 159 | 162 => (1, 1),
        160 => (1, 2),
        104 | 94 | 152 | 153 | 154 | 206 => (0, 1),
        105 | 155 => (0, 3),
        106 | 92 | 93 | 113..=116 | 147..=151 => (0, 0),
        _ => return None,
    })
}

/// Decode all instructions of a program body (the dwords after the version/length tokens).
pub fn decode_program(toks: &[u32], warnings: &mut Vec<String>) -> Vec<Instr> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < toks.len() {
        let t = toks[pos];
        let op = (t & 0x7ff) as u16;
        let mut ins = Instr { op, control: (t >> 11) & 0x1fff, ..Default::default() };
        if op == OP_CUSTOMDATA {
            let len = toks.get(pos + 1).copied().unwrap_or(0) as usize;
            if len < 2 || pos + len > toks.len() {
                warnings.push(format!("customdata at dword {} has bad length {}; stopping", pos, len));
                break;
            }
            ins.custom = Some(((t >> 11) & 0x1f, toks[pos + 2..pos + len].to_vec()));
            out.push(ins);
            pos += len;
            continue;
        }
        let len = ((t >> 24) & 0x7f) as usize;
        if len == 0 || pos + len > toks.len() {
            warnings.push(format!("instruction at dword {} (opcode {}) has bad length {}; stopping", pos, op, len));
            break;
        }
        let body = &toks[pos..pos + len];
        let mut p = 1usize;
        ins.sat = (t >> 13) & 1 != 0 && !is_decl(op) && op != 190;
        ins.test_nz = (t >> 18) & 1 != 0;
        ins.precise = ((t >> 19) & 0xf) as u8;
        let mut ext = t & 0x8000_0000 != 0;
        let mut guard = 0;
        while ext && p < len {
            let e = body[p];
            p += 1;
            match e & 0x3f {
                1 => {
                    ins.offset = [sext4(e >> 9), sext4(e >> 13), sext4(e >> 17)];
                    ins.has_offset = true;
                }
                2 => ins.ext_dim = Some((((e >> 6) & 0x1f) as u8, ((e >> 11) & 0xfff) as u16)),
                3 => {
                    ins.ext_ret = Some([((e >> 6) & 0xf) as u8, ((e >> 10) & 0xf) as u8, ((e >> 14) & 0xf) as u8, ((e >> 18) & 0xf) as u8])
                }
                _ => {}
            }
            ext = e & 0x8000_0000 != 0;
            guard += 1;
            if guard > 6 {
                break;
            }
        }
        if opname(op).is_none() {
            warnings.push(format!("unknown opcode {} at dword {}", op, pos));
            ins.bad = true;
            ins.unknown = true;
            ins.extra = body[1..].to_vec();
            out.push(ins);
            pos += len;
            continue;
        }
        let res: Result<(), String> = (|| {
            if let Some((nops, nlit)) = decl_shape(op) {
                for _ in 0..nops {
                    ins.ops.push(parse_operand(body, &mut p, 0)?);
                }
                if p + nlit > len {
                    return Err("declaration literals truncated".into());
                }
                ins.extra = body[p..len].to_vec();
            } else if op == 145 || op == 146 {
                ins.extra = body[p..].to_vec();
            } else {
                while p < len {
                    ins.ops.push(parse_operand(body, &mut p, 0)?);
                }
            }
            Ok(())
        })();
        if let Err(e) = res {
            warnings.push(format!("{}: {} (opcode {} at dword {}); printed raw", opname(op).unwrap_or("?"), e, op, pos));
            ins.bad = true;
            ins.ops.clear();
            ins.extra = body[1..].to_vec();
        }
        out.push(ins);
        pos += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_table_spot_checks() {
        for (op, n) in [(0, "add"), (53, "customdata"), (54, "mov"), (69, "sample"), (88, "dcl_resource"), (104, "dcl_temps"),
            (106, "dcl_globalFlags"), (108, "lod"), (113, "hs_decls"), (129, "rcp"), (138, "ubfe"), (143, "dcl_stream"),
            (155, "dcl_thread_group"), (163, "ld_uav_typed"), (190, "sync"), (206, "dcl_gs_instance_count"), (216, "utod")] {
            assert_eq!(opname(op), Some(n), "opcode {}", op);
        }
    }

    #[test]
    fn mov_with_immediate_and_modifiers() {
        // mov r0.xy, -v1.yxxx  : opcode 54, len 5 ; dst mask 0x3 temp 0 ; src swizzle yxxx input 1 with neg modifier
        let dst = 2 | (3 << 4) | (1 << 20);
        let src = 0x8000_0000 | 2 | (1 << 2) | (1 << 4) | (1 << 12) | (1 << 20);
        let ext = 1 | (1 << 6);
        let toks = [54 | (6 << 24), dst, 0, src, ext, 1];
        let mut w = vec![];
        let v = decode_program(&toks, &mut w);
        assert!(w.is_empty(), "{:?}", w);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].ops[0].ty, 0);
        assert_eq!(v[0].ops[0].mask(), 3);
        assert_eq!(v[0].ops[1].ty, 1);
        assert!(v[0].ops[1].neg);
        assert_eq!(v[0].ops[1].swizzle(), [1, 0, 0, 0]);
    }

    #[test]
    fn truncated_does_not_panic() {
        let mut w = vec![];
        let _ = decode_program(&[54 | (9 << 24), 0, 0], &mut w);
        assert!(!w.is_empty());
    }
}
