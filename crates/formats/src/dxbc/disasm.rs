//! fxc-like disassembly text for decoded instructions.

use super::ops::{is_decl, opname, Idx, Instr, Operand, Sel};

/// Value kind an operand is interpreted as (only used to pick how immediates print and, in the decompiler, to type expressions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    F,
    I,
    U,
    /// Raw bits (moves, selects): the type comes from the producer.
    X,
}

/// Number of leading destination operands.
pub fn ndst(op: u16) -> usize {
    match op {
        // control flow, emit, sync, nop, stores, plain atomics (the first operand is the resource), declarations
        2..=10 | 13 | 18..=23 | 31 | 44 | 48 | 58 | 62 | 63 | 76 | 164 | 166 | 168 | 169..=177 | 190 | 117..=119 | 120 => 0,
        _ if is_decl(op) => 0,
        38 | 77 | 78 | 81 | 132 | 133 | 142 => 2,
        _ => 1,
    }
}

/// Kind of the source operand `i` (counted from the first source) for opcode `op`.
pub fn src_kind(op: u16, i: usize) -> Kind {
    use Kind::*;
    match op {
        0 | 11 | 12 | 14..=17 | 24 | 25 | 26 | 27 | 28 | 29 | 47 | 49..=52 | 56 | 57 | 64..=68 | 75 | 77 | 122..=125 | 129 | 130 | 199 => F,
        // mad / compare
        30 | 33 | 34 | 35 | 36 | 37 | 38 | 39 | 40 | 43 | 32 => I,
        41 | 42 => {
            if i == 0 {
                I
            } else {
                U
            }
        }
        78 | 79 | 80 | 81 | 82 | 83 | 84 | 85 | 86 | 1 | 60 | 87 | 59 | 132 | 133 | 134 | 135 | 136 | 138 | 139 | 140 | 141 => U,
        137 => I,
        55 => {
            if i == 0 {
                U
            } else {
                X
            }
        }
        54 | 196 => X,
        69..=74 | 108 | 109 | 126 | 127 | 128 => {
            // coordinate / bias / lod / reference are float; resource and sampler operands are handled by the caller
            F
        }
        45 | 46 => I,
        13 | 3 | 5 | 8 | 31 | 63 => U,
        76 | 6 => I,
        165 | 166 | 167 | 168 => U,
        163 | 164 => U,
        169..=189 => U,
        _ => X,
    }
}

pub fn dim_name(d: u32) -> &'static str {
    match d {
        1 => "buffer",
        2 => "texture1d",
        3 => "texture2d",
        4 => "texture2dms",
        5 => "texture3d",
        6 => "texturecube",
        7 => "texture1darray",
        8 => "texture2darray",
        9 => "texture2dmsarray",
        10 => "texturecubearray",
        11 => "raw_buffer",
        12 => "structured_buffer",
        _ => "unknown_dim",
    }
}

pub fn ret_name(r: u32) -> &'static str {
    match r {
        1 => "unorm",
        2 => "snorm",
        3 => "sint",
        4 => "uint",
        5 => "float",
        6 => "mixed",
        7 => "double",
        8 => "continued",
        9 => "unused",
        _ => "?",
    }
}

pub fn sysval_name(v: u32) -> &'static str {
    match v {
        0 => "undefined",
        1 => "position",
        2 => "clip_distance",
        3 => "cull_distance",
        4 => "rendertarget_array_index",
        5 => "viewport_array_index",
        6 => "vertex_id",
        7 => "primitive_id",
        8 => "instance_id",
        9 => "is_front_face",
        10 => "sample_index",
        11 => "finalQuadUeq0EdgeTessFactor",
        12 => "finalQuadVeq0EdgeTessFactor",
        13 => "finalQuadUeq1EdgeTessFactor",
        14 => "finalQuadVeq1EdgeTessFactor",
        15 => "finalQuadUInsideTessFactor",
        16 => "finalQuadVInsideTessFactor",
        17 => "finalTriUeq0EdgeTessFactor",
        18 => "finalTriVeq0EdgeTessFactor",
        19 => "finalTriWeq0EdgeTessFactor",
        20 => "finalTriInsideTessFactor",
        21 => "finalLineDetailTessFactor",
        22 => "finalLineDensityTessFactor",
        _ => "unknown_sysval",
    }
}

const XYZW: [char; 4] = ['x', 'y', 'z', 'w'];

pub fn fmt_float(f: f32) -> String {
    if f.is_nan() {
        "1.#QNAN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "1.#INF".into() } else { "-1.#INF".into() }
    } else {
        format!("{:.6}", f)
    }
}

pub fn fmt_imm(v: u32, k: Kind) -> String {
    match k {
        Kind::F => fmt_float(f32::from_bits(v)),
        Kind::I => format!("{}", v as i32),
        Kind::U => {
            if v < 0x10000 { format!("{}", v) } else { format!("0x{:x}", v) }
        }
        Kind::X => {
            let e = (v >> 23) & 0xff;
            if v == 0 {
                "0".into()
            } else if (0x40..0xc0).contains(&e) {
                fmt_float(f32::from_bits(v))
            } else if (v as i32).unsigned_abs() < 0x10000 {
                format!("{}", v as i32)
            } else {
                format!("0x{:x}", v)
            }
        }
    }
}

fn sel_str(s: &Sel) -> String {
    match s {
        Sel::None => String::new(),
        Sel::Mask(m) => {
            if *m == 0 {
                return String::new();
            }
            let mut o = String::from(".");
            for c in 0..4 {
                if m & (1 << c) != 0 {
                    o.push(XYZW[c]);
                }
            }
            o
        }
        Sel::Swz(w) => format!(".{}{}{}{}", XYZW[w[0] as usize & 3], XYZW[w[1] as usize & 3], XYZW[w[2] as usize & 3], XYZW[w[3] as usize & 3]),
        Sel::One(c) => format!(".{}", XYZW[*c as usize & 3]),
    }
}

fn idx_str(i: &Idx, kind: Kind) -> String {
    match i {
        Idx::Imm(v) => format!("{}", v),
        Idx::Rel(v, r) => format!("{} + {}", operand_str(r, kind), v).replace("  ", " "),
    }
}

pub fn operand_str(o: &Operand, k: Kind) -> String {
    let i0 = o.idx.first().map(|i| idx_str(i, Kind::U)).unwrap_or_default();
    let rest: String = o.idx.iter().skip(1).map(|i| format!("[{}]", idx_str(i, Kind::U))).collect();
    let body = match o.ty {
        0 => format!("r{}", i0),
        1 => {
            if o.idx.len() >= 2 {
                format!("v[{}]{}", i0, rest)
            } else {
                format!("v{}", i0)
            }
        }
        2 => format!("o{}", i0),
        3 => format!("x{}{}", i0, rest),
        4 | 5 => {
            let pre = if o.ty == 4 { "l" } else { "d" };
            let parts: Vec<String> = if o.ty == 4 {
                o.imm.iter().map(|v| fmt_imm(*v, k)).collect()
            } else {
                o.imm
                    .chunks(2)
                    .map(|c| if c.len() == 2 { format!("{}", f64::from_bits(c[0] as u64 | (c[1] as u64) << 32)) } else { String::new() })
                    .collect()
            };
            format!("{}({})", pre, parts.join(", "))
        }
        6 => format!("s{}{}", i0, rest),
        7 => format!("t{}{}", i0, rest),
        8 => {
            if o.idx.len() == 3 {
                format!("CB{}[{}][{}]", i0, idx_str(&o.idx[1], Kind::U), idx_str(&o.idx[2], Kind::U))
            } else {
                format!("cb{}{}", i0, rest)
            }
        }
        9 => format!("icb[{}]", i0),
        10 => format!("label{}", i0),
        11 => "vPrim".into(),
        12 => "oDepth".into(),
        13 => "null".into(),
        14 => "rasterizer".into(),
        15 => "oMask".into(),
        16 => format!("m{}", i0),
        17 => format!("fb{}", i0),
        18 => format!("ft{}", i0),
        19 => format!("fp{}", i0),
        20 => format!("fi{}", i0),
        21 => format!("fo{}", i0),
        22 => "vOutputControlPointID".into(),
        23 => "vForkInstanceID".into(),
        24 => "vJoinInstanceID".into(),
        25 => format!("vicp[{}]{}", i0, rest),
        26 => format!("vocp[{}]{}", i0, rest),
        27 => format!("vpc{}", i0),
        28 => "vDomain".into(),
        29 => "this".into(),
        30 => format!("u{}{}", i0, rest),
        31 => format!("g{}{}", i0, rest),
        32 => "vThreadID".into(),
        33 => "vThreadGroupID".into(),
        34 => "vThreadIDInGroup".into(),
        35 => "vCoverage".into(),
        36 => "vThreadIDInGroupFlattened".into(),
        37 => "vGSInstanceID".into(),
        38 => "oDepthGE".into(),
        39 => "oDepthLE".into(),
        40 => "cycleCounter".into(),
        t => format!("operand_type_{}[{}]", t, i0),
    };
    let sel = if o.ty == 4 || o.ty == 5 { String::new() } else { sel_str(&o.sel) };
    let mut s = format!("{}{}", body, sel);
    if o.abs {
        s = format!("|{}|", s);
    }
    if o.neg {
        s = format!("-{}", s);
    }
    s
}

fn interp_name(v: u32) -> &'static str {
    match v {
        1 => "constant",
        2 => "linear",
        3 => "linearCentroid",
        4 => "linearNoperspective",
        5 => "linearNoperspectiveCentroid",
        6 => "linearSample",
        7 => "linearNoperspectiveSample",
        _ => "undefined",
    }
}

fn ret_tuple(w: u32) -> String {
    format!("({},{},{},{})", ret_name(w & 15), ret_name((w >> 4) & 15), ret_name((w >> 8) & 15), ret_name((w >> 12) & 15))
}

/// Resource declarations seen so far (t# -> dimension code and return type word), used to annotate sample/ld mnemonics.
#[derive(Default)]
pub struct ResInfo {
    pub dims: std::collections::HashMap<u32, (u32, u32, u32)>,
}

pub fn collect_res(prog: &[Instr]) -> ResInfo {
    let mut r = ResInfo::default();
    for i in prog {
        match i.op {
            88 => {
                if let (Some(o), Some(rt)) = (i.ops.first(), i.extra.first()) {
                    if let Some(n) = o.index(0) {
                        r.dims.insert(n, ((i.control & 31), *rt, (i.control >> 5) & 0x7f));
                    }
                }
            }
            161 => {
                if let Some(n) = i.ops.first().and_then(|o| o.index(0)) {
                    r.dims.insert(n, (11, 0x5555, 0));
                }
            }
            162 => {
                if let Some(n) = i.ops.first().and_then(|o| o.index(0)) {
                    r.dims.insert(n, (12, 0x5555, 0));
                }
            }
            _ => {}
        }
    }
    r
}

fn flag_names(c: u32, names: &[&str]) -> String {
    let v: Vec<&str> = names.iter().enumerate().filter(|(k, _)| c & (1 << k) != 0).map(|(_, n)| *n).collect();
    if v.is_empty() { "none".into() } else { v.join(", ") }
}

fn name_suffix(i: &Instr, res: &ResInfo) -> String {
    // sample/ld style annotations: aoffimmi(u,v,w) then _indexable(dim)(types)
    let mut s = String::new();
    if i.has_offset {
        s.push_str(&format!("_aoffimmi({},{},{})", i.offset[0], i.offset[1], i.offset[2]));
    }
    let res_op = match i.op {
        45 | 46 | 69..=74 | 108 | 109 | 126..=128 | 61 | 111 | 121 | 165 | 167 => i.ops.iter().find(|o| o.ty == 7),
        163 => i.ops.iter().find(|o| o.ty == 30),
        _ => None,
    };
    if let Some(o) = res_op {
        if o.ty == 7 {
            if let Some((d, rt, ms)) = o.index(0).and_then(|n| res.dims.get(&n)) {
                let dn = dim_name(*d);
                let dn = if *d == 4 || *d == 9 { format!("{}({})", dn, ms) } else { dn.to_string() };
                match d {
                    12 => s.push_str(&format!("_indexable(structured_buffer, stride={})", i.ext_dim.map(|x| x.1).unwrap_or(0))),
                    11 => s.push_str("_indexable(raw_buffer)"),
                    _ => s.push_str(&format!("_indexable({}){}", dn, ret_tuple(*rt))),
                }
            }
        }
    }
    s
}

/// Disassemble one instruction (without indentation or newline).
pub fn instr_text(i: &Instr, res: &ResInfo) -> String {
    let name = match opname(i.op) {
        Some(n) => n.to_string(),
        None => return format!("unknown_op_{} 0x{:08x?}", i.op, i.extra),
    };
    if let Some((class, data)) = &i.custom {
        return match class {
            3 => {
                let mut s = String::from("dcl_immediateConstantBuffer {\n");
                let n = data.len() / 4;
                for (k, c) in data.chunks(4).enumerate() {
                    let v: Vec<String> = c.iter().map(|v| fmt_imm(*v, Kind::X)).collect();
                    s.push_str(&format!("  {{ {}}}{}\n", v.join(", "), if k + 1 < n { "," } else { "" }));
                }
                s.push('}');
                s
            }
            0 => format!("// comment ({} dwords)", data.len()),
            1 => format!("// debuginfo ({} dwords)", data.len()),
            c => format!("customdata class {} ({} dwords)", c, data.len()),
        };
    }
    if i.bad {
        let v: Vec<String> = i.extra.iter().map(|d| format!("0x{:08x}", d)).collect();
        return format!("{} {}", name, v.join(" "));
    }
    let c = i.control;
    let ops_str = |k: &dyn Fn(usize) -> Kind| -> Vec<String> {
        let nd = ndst(i.op);
        i.ops.iter().enumerate().map(|(n, o)| operand_str(o, if n < nd { Kind::X } else { k(n - nd) })).collect()
    };
    let ex = |n: usize| i.extra.get(n).copied().unwrap_or(0);
    let join = |v: Vec<String>| v.join(", ");
    let kf = |n: usize| src_kind(i.op, n);
    match i.op {
        106 => format!("{} {}", name, flag_names(c, &["refactoringAllowed", "enableDoublePrecisionFloatOps", "forceEarlyDepthStencil", "enableRawAndStructuredBuffers", "skipOptimization", "enableMinimumPrecision", "enable11_1DoubleExtensions", "enable11_1ShaderExtensions"])),
        89 => format!("dcl_constantbuffer {}, {}", {
            let mut o = i.ops.clone();
            for x in o.iter_mut() {
                x.sel = Sel::None;
            }
            o.iter().map(|x| operand_str(x, Kind::U)).collect::<Vec<_>>().join(", ")
        }, if c & 1 != 0 { "dynamicIndexed" } else { "immediateIndexed" }),
        90 => format!("dcl_sampler {}, mode_{}", join(ops_str(&kf)), ["default", "comparison", "mono"].get((c & 15) as usize).unwrap_or(&"?")),
        88 => {
            let d = c & 31;
            let dn = if d == 4 || d == 9 { format!("{}({})", dim_name(d), (c >> 5) & 0x7f) } else { dim_name(d).to_string() };
            format!("dcl_resource_{} {} {}", dn, ret_tuple(ex(0)), join(ops_str(&kf)))
        }
        104 => format!("dcl_temps {}", ex(0)),
        105 => format!("dcl_indexableTemp x{}[{}], {}", ex(0), ex(1), ex(2)),
        95 | 101 => format!("{} {}", name, join(ops_str(&kf))),
        98 => format!("dcl_input_ps {} {}", interp_name(c & 15), join(ops_str(&kf))),
        96 | 102 | 103 | 97 => format!("{} {}, {}", name, join(ops_str(&kf)), sysval_name(ex(0))),
        99 | 100 => format!("{} {} {}, {}", name, interp_name(c & 15), join(ops_str(&kf)), sysval_name(ex(0))),
        91 => format!("dcl_index_range {}, {}", join(ops_str(&kf)), ex(0)),
        92 => format!("dcl_outputtopology {}", match c & 0x7f { 1 => "pointlist", 3 => "linestrip", 5 => "trianglestrip", _ => "undefined" }),
        93 => format!("dcl_inputprimitive {}", match c & 0x7f { 1 => "point".to_string(), 2 => "line".into(), 3 => "triangle".into(), 6 => "lineadj".into(), 7 => "triangleadj".into(), n @ 8..=40 => format!("patch{}", n - 7), _ => "undefined".into() }),
        94 => format!("dcl_maxout {}", ex(0)),
        155 => format!("dcl_thread_group {}, {}, {}", ex(0), ex(1), ex(2)),
        147 => format!("dcl_input_control_point_count {}", c & 0x3f),
        148 => format!("dcl_output_control_point_count {}", c & 0x3f),
        149 => format!("dcl_tessellator_domain domain_{}", match c & 7 { 1 => "isoline", 2 => "tri", 3 => "quad", _ => "undefined" }),
        150 => format!("dcl_tessellator_partitioning partitioning_{}", match c & 7 { 1 => "integer", 2 => "pow2", 3 => "fractional_odd", 4 => "fractional_even", _ => "undefined" }),
        151 => format!("dcl_tessellator_output_primitive output_{}", match c & 7 { 1 => "point", 2 => "line", 3 => "triangle_cw", 4 => "triangle_ccw", _ => "undefined" }),
        152 => format!("dcl_hs_max_tessfactor l({})", fmt_float(f32::from_bits(ex(0)))),
        153 | 154 | 206 => format!("{} {}", name, ex(0)),
        156 => format!("dcl_uav_typed_{}{} {} {}", dim_name(c & 15), if c & 0x8000 != 0 { "_glc" } else { "" }, ret_tuple(ex(0)), join(ops_str(&kf))),
        157 => format!("dcl_uav_raw{} {}", if c & 0x8000 != 0 { "_glc" } else { "" }, join(ops_str(&kf))),
        158 => format!("dcl_uav_structured{} {}, {}", if c & 0x8000 != 0 { "_glc" } else { "" }, join(ops_str(&kf)), ex(0)),
        159 => format!("dcl_tgsm_raw {}, {}", join(ops_str(&kf)), ex(0)),
        160 => format!("dcl_tgsm_structured {}, {}, {}", join(ops_str(&kf)), ex(0), ex(1)),
        161 => format!("dcl_resource_raw {}", join(ops_str(&kf))),
        162 => format!("dcl_resource_structured {}, {}", join(ops_str(&kf)), ex(0)),
        113..=116 | 18 | 21..=23 | 62 | 48 | 2 | 7 | 9 | 10 | 19 | 20 | 58 | 204 | 205 => name,
        190 => {
            // EMPIRICAL layout (public header): bit 11 threads-in-group, 12 group shared memory, 13 uav group, 14 uav global.
            let mut s = String::from("sync");
            if c & 8 != 0 {
                s.push_str("_uglobal");
            }
            if c & 4 != 0 {
                s.push_str("_ugroup");
            }
            if c & 2 != 0 {
                s.push_str("_g");
            }
            if c & 1 != 0 {
                s.push_str("_t");
            }
            s
        }
        _ => {
            let mut n = name;
            if matches!(i.op, 31 | 3 | 8 | 5 | 63 | 13) {
                n.push_str(if i.test_nz { "_nz" } else { "_z" });
            }
            if i.op == 61 {
                n.push_str(["", "_rcpFloat", "_uint"].get((c & 3) as usize).unwrap_or(&""));
            }
            if i.sat {
                n.push_str("_sat");
            }
            n.push_str(&name_suffix(i, res));
            if i.precise != 0 {
                let mut p = String::new();
                for k in 0..4 {
                    if i.precise & (1 << k) != 0 {
                        p.push(XYZW[k]);
                    }
                }
                n.push_str(&format!(" [precise({})]", p));
            }
            // resource/sampler operands of sample-class instructions print without kinds
            let ops = ops_str(&kf);
            if ops.is_empty() { n } else { format!("{} {}", n, join(ops)) }
        }
    }
}

/// Disassemble a whole program with block indentation.
pub fn disasm_program(prog: &[Instr], header: &str) -> String {
    let res = collect_res(prog);
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    let mut depth: usize = 0;
    for i in prog {
        match i.op {
            21 | 22 | 23 | 18 => depth = depth.saturating_sub(1),
            _ => {}
        }
        let t = instr_text(i, &res);
        let pad = "  ".repeat(depth);
        for (n, line) in t.lines().enumerate() {
            if n == 0 {
                out.push_str(&pad);
            }
            out.push_str(line);
            out.push('\n');
        }
        match i.op {
            31 | 48 | 76 | 18 => depth += 1,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::ops::decode_program;

    fn op_tok(op: u32, len: u32, ctl: u32) -> u32 {
        op | (ctl << 11) | (len << 24)
    }

    #[test]
    fn prints_basic_instructions() {
        // mov r0.xy, v1.yxxx ; add_sat r1.xyzw, r0.xyxy, l(1,2,3,4)? kept simple: mov and ret
        let dst = (2) | (0 << 2) | (0x3 << 4) | (0 << 12) | (1 << 20);
        let src = (2) | (1 << 2) | (0b0000_0001 << 4) | (1 << 12) | (1 << 20); // swizzle: x=1(y) y=0(x) z=0 w=0
        let toks = [op_tok(54, 5, 0), dst, 0, src, 1, op_tok(62, 1, 0)];
        let mut w = vec![];
        let p = decode_program(&toks, &mut w);
        assert!(w.is_empty(), "{:?}", w);
        let t = disasm_program(&p, "vs_5_0");
        assert!(t.contains("mov r0.xy, v1.yxxx"), "{}", t);
        assert!(t.trim_end().ends_with("ret"), "{}", t);
    }

    #[test]
    fn immediate_and_saturate() {
        // mul_sat r0.x, l(0.5), l(1.0)
        let dst = (2) | (0x1 << 4) | (1 << 20);
        let imm = 1 | (4 << 12);
        let toks = [op_tok(56, 7, 0x2000 >> 11), dst, 0, imm, 0.5f32.to_bits(), imm, 1.0f32.to_bits()];
        let mut w = vec![];
        let p = decode_program(&toks, &mut w);
        assert!(w.is_empty(), "{:?}", w);
        let t = disasm_program(&p, "ps_5_0");
        assert!(t.contains("mul_sat r0.x, l(0.500000), l(1.000000)"), "{}", t);
    }
}
