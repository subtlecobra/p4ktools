//! Hand-assembled DXBC blobs: decoder, disassembler and decompiler end to end without any game files.

use super::*;

fn dst(ty: u32, idx: u32, mask: u32) -> Vec<u32> {
    vec![2 | (mask << 4) | (ty << 12) | (1 << 20), idx]
}
fn src(ty: u32, idx: u32, swz: [u32; 4]) -> Vec<u32> {
    vec![2 | (1 << 2) | ((swz[0] | swz[1] << 2 | swz[2] << 4 | swz[3] << 6) << 4) | (ty << 12) | (1 << 20), idx]
}
fn sel1(ty: u32, idx: u32, c: u32) -> Vec<u32> {
    vec![2 | (2 << 2) | (c << 4) | (ty << 12) | (1 << 20), idx]
}
fn imm(v: [f32; 4]) -> Vec<u32> {
    vec![2 | (4 << 12), v[0].to_bits(), v[1].to_bits(), v[2].to_bits(), v[3].to_bits()]
}
fn immi(v: [u32; 4]) -> Vec<u32> {
    vec![2 | (4 << 12), v[0], v[1], v[2], v[3]]
}
fn cb(slot: u32, reg: u32, swz: [u32; 4]) -> Vec<u32> {
    vec![2 | (1 << 2) | ((swz[0] | swz[1] << 2 | swz[2] << 4 | swz[3] << 6) << 4) | (8 << 12) | (2 << 20), slot, reg]
}
/// Operand without component selection (resources, samplers).
fn plain(ty: u32, idx: u32) -> Vec<u32> {
    vec![(ty << 12) | (1 << 20), idx]
}
fn inst(op: u32, ctl: u32, operands: &[Vec<u32>]) -> Vec<u32> {
    let n: usize = 1 + operands.iter().map(|o| o.len()).sum::<usize>();
    let mut v = vec![op | (ctl << 11) | ((n as u32) << 24)];
    for o in operands {
        v.extend(o);
    }
    v
}
fn inst_lit(op: u32, ctl: u32, operands: &[Vec<u32>], lits: &[u32]) -> Vec<u32> {
    let mut v = inst(op, ctl, operands);
    v.extend(lits);
    v[0] = (v[0] & 0x00ff_ffff) | ((v.len() as u32) << 24);
    v
}

const XYZW: [u32; 4] = [0, 1, 2, 3];
const XYXY: [u32; 4] = [0, 1, 0, 1];
const XXXX: [u32; 4] = [0, 0, 0, 0];

fn container(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = b"DXBC".to_vec();
    out.extend([0u8; 16]);
    out.extend(1u32.to_le_bytes());
    let total = 32 + 4 * chunks.len() + chunks.iter().map(|c| 8 + c.1.len()).sum::<usize>();
    out.extend((total as u32).to_le_bytes());
    out.extend((chunks.len() as u32).to_le_bytes());
    let mut off = 32 + 4 * chunks.len();
    for c in chunks {
        out.extend((off as u32).to_le_bytes());
        off += 8 + c.1.len();
    }
    for c in chunks {
        out.extend(c.0);
        out.extend((c.1.len() as u32).to_le_bytes());
        out.extend(&c.1);
    }
    out
}

fn shex(stage: u32, instrs: &[Vec<u32>]) -> Vec<u8> {
    let body: Vec<u32> = instrs.iter().flatten().copied().collect();
    let mut w = vec![(stage << 16) | (5 << 4), (body.len() + 2) as u32];
    w.extend(body);
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Signature chunk (24-byte entries): (name, index, sysvalue, component type, register, mask, rw mask).
fn sig(entries: &[(&str, u32, u32, u32, u32, u32, u32)]) -> Vec<u8> {
    let mut strings = Vec::new();
    let mut offs = Vec::new();
    let base = 8 + 24 * entries.len();
    for e in entries {
        offs.push(base + strings.len());
        strings.extend(e.0.as_bytes());
        strings.push(0);
    }
    let mut w: Vec<u32> = vec![entries.len() as u32, 8];
    for (e, o) in entries.iter().zip(offs) {
        w.extend([o as u32, e.1, e.2, e.3, e.4, e.5 | (e.6 << 8)]);
    }
    let mut out: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    out.extend(strings);
    out
}

/// SM5 RDEF with one cbuffer `Globals` (slot 0) holding `float4 gColor` (used) and `float gScale`.
fn rdef_globals() -> Vec<u8> {
    let mut d: Vec<u8> = Vec::new();
    let mut put = |d: &mut Vec<u8>, v: &[u32]| d.extend(v.iter().flat_map(|x| x.to_le_bytes()));
    // header (7 dwords) + RD11 block (8 dwords) = 60 bytes
    let cb_off = 60u32;
    let var_off = cb_off + 24;
    let ty_off = var_off + 80;
    let res_off = ty_off + 72;
    let str_off = res_off + 32;
    let (n_globals, n_color, n_scale, n_float4, n_float, n_creator) = (str_off, str_off + 8, str_off + 15, str_off + 22, str_off + 29, str_off + 35);
    put(&mut d, &[1, cb_off, 1, res_off, 0x0500, 0, n_creator]);
    put(&mut d, &[0x31315244, 60, 24, 32, 40, 36, 12, 0]);
    // cbuffer desc: name, var count, var offset, size, flags, type
    put(&mut d, &[n_globals, 2, var_off, 32, 0, 0]);
    // variables: name, offset, size, flags, type, default, 4 texture/sampler dwords
    put(&mut d, &[n_color, 0, 16, 2, ty_off, 0, 0, 0, 0, 0]);
    put(&mut d, &[n_scale, 16, 4, 0, ty_off + 36, 0, 0, 0, 0, 0]);
    // types: class|type<<16, rows|cols<<16, elements|members<<16, member offset, 4 unused, name
    put(&mut d, &[1 | (3 << 16), 1 | (4 << 16), 0, 0, 0, 0, 0, 0, n_float4]);
    put(&mut d, &[0 | (3 << 16), 1 | (1 << 16), 0, 0, 0, 0, 0, 0, n_float]);
    // resource binding: name, type, ret, dim, samples, bind point, count, flags
    put(&mut d, &[n_globals, 0, 0, 0, 0, 0, 1, 0]);
    d.extend(b"Globals\0gColor\0gScale\0float4\0float\0test\0");
    // fix up the string offsets used above (they are relative to the strings block start)
    assert_eq!(d.len() as u32, str_off + 40);
    assert_eq!(&d[(n_scale as usize)..(n_scale as usize + 6)], b"gScale");
    assert_eq!(&d[(n_float as usize)..(n_float as usize + 5)], b"float");
    d
}

fn run(chunks: &[(&[u8; 4], Vec<u8>)]) -> DxbcShader {
    let blob = container(chunks);
    decode(&blob).expect("decode")
}

#[test]
fn stripped_pixel_shader_degrades_to_register_names() {
    let prog = vec![
        inst_lit(98, 2, &[dst(1, 1, 3)], &[]),
        inst(101, 0, &[dst(2, 0, 15)]),
        inst_lit(104, 0, &[], &[1]),
        inst(0, 0, &[dst(0, 0, 1), sel1(1, 1, 0), sel1(1, 1, 1)]),
        inst(50, 0, &[dst(2, 0, 15), src(0, 0, XXXX), imm([2.0; 4]), imm([1.0; 4])]),
        inst(62, 0, &[]),
    ];
    let s = run(&[(b"SHEX", shex(0, &prog))]);
    assert_eq!((s.stage.as_str(), s.model.as_str()), ("ps", "5_0"));
    assert!(s.disassembly.contains("dcl_input_ps linear v1.xy"), "{}", s.disassembly);
    assert!(s.disassembly.contains("add r0.x, v1.x, v1.y"), "{}", s.disassembly);
    assert!(s.disassembly.contains("mad o0.xyzw, r0.xxxx, l(2.000000, 2.000000, 2.000000, 2.000000), l(1.000000"), "{}", s.disassembly);
    assert!(s.hlsl.contains("o.v0 = (i.v1.x + i.v1.y).xxxx * (float4)2.0 + (float4)1.0;"), "{}", s.hlsl);
    assert!(s.warnings.is_empty(), "{:?}", s.warnings);
}

#[test]
fn control_flow_keeps_multiply_written_registers() {
    let prog = vec![
        inst(98, 2, &[dst(1, 0, 15)]),
        inst(101, 0, &[dst(2, 0, 15)]),
        inst_lit(104, 0, &[], &[2]),
        inst(49, 0, &[dst(0, 0, 1), sel1(1, 0, 0), imm([0.5; 4])]),
        inst(54, 0, &[dst(0, 1, 15), src(1, 0, XYZW)]),
        inst(31, 1 << 7, &[sel1(0, 0, 0)]),
        inst(0, 0, &[dst(0, 1, 1), sel1(0, 1, 0), imm([1.0; 4])]),
        inst(18, 0, &[]),
        inst(56, 0, &[dst(0, 1, 1), sel1(0, 1, 0), imm([2.0; 4])]),
        inst(21, 0, &[]),
        inst(54, 0, &[dst(2, 0, 15), src(0, 1, XYZW)]),
        inst(62, 0, &[]),
    ];
    let s = run(&[(b"SHEX", shex(0, &prog))]);
    let h = &s.hlsl;
    assert!(h.contains("if (i.v0.x < 0.5) {"), "{}", h);
    assert!(h.contains("r1.x = r1.x + 1.0;") && h.contains("} else {") && h.contains("r1.x = r1.x * 2.0;"), "{}", h);
    assert!(h.contains("r1 = i.v0;") && h.contains("o.v0 = r1;"), "{}", h);
    assert!(s.disassembly.contains("if_nz r0.x") && s.disassembly.contains("endif"), "{}", s.disassembly);
}

#[test]
fn integer_loop_is_typed_and_comparison_folded_into_break() {
    let prog = vec![
        inst(101, 0, &[dst(2, 0, 15)]),
        inst_lit(104, 0, &[], &[2]),
        inst(54, 0, &[dst(0, 0, 1), immi([0; 4])]),
        inst(48, 0, &[]),
        inst(33, 0, &[dst(0, 0, 2), sel1(0, 0, 0), immi([4; 4])]),
        inst(3, 1 << 7, &[sel1(0, 0, 1)]),
        inst(30, 0, &[dst(0, 0, 1), sel1(0, 0, 0), immi([1; 4])]),
        inst(22, 0, &[]),
        inst(43, 0, &[dst(0, 1, 1), sel1(0, 0, 0)]),
        inst(54, 0, &[dst(2, 0, 15), src(0, 1, XXXX)]),
        inst(62, 0, &[]),
    ];
    let s = run(&[(b"SHEX", shex(0, &prog))]);
    let h = &s.hlsl;
    assert!(h.contains("int4 r0;"), "{}", h);
    assert!(h.contains("while (true) {") && h.contains("if (r0.x >= 4) break;") && h.contains("r0.x = r0.x + 1;"), "{}", h);
    assert!(h.contains("o.v0 = float(r0.x).xxxx;"), "{}", h);
}

#[test]
fn unknown_opcode_is_reported_and_decoding_continues() {
    let prog = vec![inst(54, 0, &[dst(0, 0, 1), imm([1.0; 4])]), vec![500 | (2 << 24), 0xdead_beef], inst(62, 0, &[])];
    let s = run(&[(b"SHEX", shex(1, &prog))]);
    assert!(s.warnings.iter().any(|w| w.contains("unknown opcode 500")), "{:?}", s.warnings);
    assert!(s.disassembly.contains("unknown_op_500") && s.disassembly.trim_end().ends_with("ret"), "{}", s.disassembly);
    assert_eq!(s.stage, "vs");
}

#[test]
fn reflection_names_cbuffer_variables_and_signature_fields() {
    let prog = vec![
        inst(89, 0, &[vec![2 | (1 << 2) | (0xe4 << 4) | (8 << 12) | (2 << 20), 0, 2]]),
        inst(98, 2, &[dst(1, 0, 3)]),
        inst(101, 0, &[dst(2, 0, 15)]),
        inst_lit(104, 0, &[], &[1]),
        inst(56, 0, &[dst(0, 0, 15), cb(0, 0, XYZW), src(1, 0, XYXY)]),
        inst(56, 0, &[dst(2, 0, 15), src(0, 0, XYZW), cb(0, 1, XXXX)]),
        inst(62, 0, &[]),
    ];
    let s = run(&[
        (b"RDEF", rdef_globals()),
        (b"ISGN", sig(&[("TEXCOORD", 0, 0, 3, 0, 3, 3)])),
        (b"OSGN", sig(&[("SV_Target", 0, 0, 3, 0, 15, 0)])),
        (b"SHEX", shex(0, &prog)),
    ]);
    let h = &s.hlsl;
    assert!(h.contains("cbuffer Globals : register(b0)"), "{}", h);
    assert!(h.contains("float4 gColor; // offset 0") && h.contains("float gScale; // offset 16"), "{}", h);
    assert!(h.contains("float2 TEXCOORD0 : TEXCOORD0; // v0.xy"), "{}", h);
    assert!(h.contains("o.SV_Target = gColor * i.TEXCOORD0.xyxy * gScale.xxxx;"), "{}", h);
    assert!(s.disassembly.contains("// cbuffer Globals"), "{}", s.disassembly);
}

#[test]
fn texture_sample_becomes_method_call() {
    let prog = vec![
        inst_lit(88, 3, &[plain(7, 0)], &[0x5555]),
        inst(90, 0, &[plain(6, 0)]),
        inst(98, 2, &[dst(1, 0, 3)]),
        inst(101, 0, &[dst(2, 0, 15)]),
        inst_lit(104, 0, &[], &[1]),
        inst(69, 0, &[dst(0, 0, 15), src(1, 0, XYXY), src(7, 0, XYZW), plain(6, 0)]),
        inst(54, 0, &[dst(2, 0, 15), src(0, 0, XYZW)]),
        inst(62, 0, &[]),
    ];
    let s = run(&[(b"SHEX", shex(0, &prog))]);
    assert!(s.disassembly.contains("sample_indexable(texture2d)(float,float,float,float) r0.xyzw, v0.xyxy, t0.xyzw, s0"), "{}", s.disassembly);
    assert!(s.hlsl.contains("o.v0 = t0.Sample(s0, i.v0.xy);"), "{}", s.hlsl);
    assert!(s.hlsl.contains("Texture2D<float4> t0 : register(t0);"), "{}", s.hlsl);
}

#[test]
fn mutated_blobs_never_panic() {
    let prog = vec![
        inst(98, 2, &[dst(1, 0, 3)]),
        inst(101, 0, &[dst(2, 0, 15)]),
        inst(31, 1 << 7, &[sel1(1, 0, 0)]),
        inst(56, 0, &[dst(2, 0, 15), cb(0, 0, XYZW), src(1, 0, XYXY)]),
        inst(21, 0, &[]),
        inst(62, 0, &[]),
    ];
    let base = container(&[(b"RDEF", rdef_globals()), (b"ISGN", sig(&[("TEXCOORD", 0, 0, 3, 0, 3, 3)])), (b"SHEX", shex(0, &prog))]);
    let mut x = 12345u32;
    for _ in 0..3000 {
        let mut b = base.clone();
        for _ in 0..1 + (x % 4) {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let pos = (x >> 8) as usize % b.len();
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            b[pos] = (x >> 16) as u8;
        }
        if x % 7 == 0 {
            b.truncate(x as usize % b.len());
        }
        let _ = decode(&b);
    }
}
