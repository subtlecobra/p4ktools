//! Hand-assembled modules: each pins one rule of the decompiler (structuring, phi edges, memory order) so a regression shows as text.

use super::table::INSTS;
use super::*;

/// A tiny assembler: `op("OpFAdd", &[..])`.
#[derive(Default)]
struct Asm {
    w: Vec<u32>,
}

impl Asm {
    fn op(&mut self, name: &str, o: &[u32]) -> &mut Self {
        let i = INSTS.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{}", name));
        self.w.push(((o.len() as u32 + 1) << 16) | i.op as u32);
        self.w.extend_from_slice(o);
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let mut all = vec![MAGIC, 0x0001_0500, 0x000e_0000, 1000, 0];
        all.extend_from_slice(&self.w);
        all.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

const VOID: u32 = 1;
const FLOAT: u32 = 2;
const BOOL: u32 = 3;
const FNTY: u32 = 4;
const PF: u32 = 5; // pointer to Private float
const ONE: u32 = 6;
const TWO: u32 = 7;
const YES: u32 = 8;
const PA: u32 = 9; // private variables
const PB: u32 = 10;
const MAIN: u32 = 20;

fn prelude() -> Asm {
    let mut a = Asm::default();
    a.op("OpCapability", &[1]);
    a.op("OpMemoryModel", &[0, 1]);
    a.op("OpEntryPoint", &[5, MAIN, u32::from_le_bytes(*b"main"), 0]);
    a.op("OpName", &[PA, u32::from_le_bytes(*b"a\0\0\0")]);
    a.op("OpName", &[PB, u32::from_le_bytes(*b"b\0\0\0")]);
    a.op("OpTypeVoid", &[VOID]);
    a.op("OpTypeFloat", &[FLOAT, 32]);
    a.op("OpTypeBool", &[BOOL]);
    a.op("OpTypeFunction", &[FNTY, VOID]);
    a.op("OpTypePointer", &[PF, 6, FLOAT]);
    a.op("OpConstant", &[FLOAT, ONE, 1f32.to_bits()]);
    a.op("OpConstant", &[FLOAT, TWO, 2f32.to_bits()]);
    a.op("OpConstantTrue", &[BOOL, YES]);
    a.op("OpVariable", &[PF, PA, 6]);
    a.op("OpVariable", &[PF, PB, 6]);
    a.op("OpFunction", &[VOID, MAIN, 0, FNTY]);
    a
}

fn text(a: &Asm) -> String {
    let s = decode(&a.bytes()).unwrap();
    assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    assert!(scope_errors(&s.hlsl).is_empty(), "{}", s.hlsl);
    assert!(s.disassembly.contains("OpFunctionEnd"));
    s.hlsl
}

fn before(t: &str, a: &str, b: &str) {
    let (x, y) = (t.find(a).unwrap_or_else(|| panic!("no {:?} in\n{}", a, t)), t.find(b).unwrap_or_else(|| panic!("no {:?} in\n{}", b, t)));
    assert!(x < y, "{:?} should precede {:?} in\n{}", a, b, t);
}

#[test]
fn if_else_with_a_phi_assigns_on_each_edge() {
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpSelectionMerge", &[33, 0]).op("OpBranchConditional", &[YES, 31, 32]);
    a.op("OpLabel", &[31]).op("OpBranch", &[33]);
    a.op("OpLabel", &[32]).op("OpBranch", &[33]);
    a.op("OpLabel", &[33]).op("OpPhi", &[FLOAT, 40, ONE, 31, TWO, 32]).op("OpStore", &[PA, 40]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let t = text(&a);
    before(&t, "if (true) {", "_40 = 1.0;");
    before(&t, "_40 = 1.0;", "} else {");
    before(&t, "} else {", "_40 = 2.0;");
    before(&t, "_40 = 2.0;", "a = _40;");
}

#[test]
fn a_header_edge_to_the_merge_still_assigns_its_phi() {
    // the false edge goes straight to the merge: its phi value must be assigned on that edge only
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpSelectionMerge", &[33, 0]).op("OpBranchConditional", &[YES, 31, 33]);
    a.op("OpLabel", &[31]).op("OpBranch", &[33]);
    a.op("OpLabel", &[33]).op("OpPhi", &[FLOAT, 40, ONE, 31, TWO, 30]).op("OpStore", &[PA, 40]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let t = text(&a);
    before(&t, "} else {", "_40 = 2.0;");
}

#[test]
fn loop_runs_its_continue_block_and_breaks_at_the_merge() {
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpBranch", &[31]);
    a.op("OpLabel", &[31]).op("OpLoopMerge", &[34, 33, 0]).op("OpBranch", &[32]);
    a.op("OpLabel", &[32]).op("OpBranchConditional", &[YES, 33, 34]);
    a.op("OpLabel", &[33]).op("OpStore", &[PB, ONE]).op("OpBranch", &[31]);
    a.op("OpLabel", &[34]).op("OpStore", &[PA, TWO]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let t = text(&a);
    before(&t, "while (true) {", "break;");
    before(&t, "while (true) {", "b = 1.0;");
    // the merge comes after the loop's closing brace
    let close = t.rfind("    }\n    a = 2.0;").expect(&t);
    assert!(t[..close].contains("b = 1.0;"), "{}", t);
}

#[test]
fn swapping_phis_read_their_old_values() {
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpBranch", &[31]);
    a.op("OpLabel", &[31]);
    a.op("OpPhi", &[FLOAT, 40, ONE, 30, 41, 32]).op("OpPhi", &[FLOAT, 41, TWO, 30, 40, 32]);
    a.op("OpLoopMerge", &[33, 32, 0]).op("OpBranchConditional", &[YES, 32, 33]);
    a.op("OpLabel", &[32]).op("OpBranch", &[31]);
    a.op("OpLabel", &[33]).op("OpStore", &[PA, 40]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let t = text(&a);
    assert!(t.contains("_old"), "{}", t);
    // the second assignment must read the copy, not the phi just overwritten
    assert!(t.contains("_41 = _40_old;") || t.contains("_40 = _41_old;"), "{}", t);
}

#[test]
fn reads_do_not_move_across_writes() {
    let mut a = prelude();
    a.op("OpLabel", &[30]);
    a.op("OpLoad", &[FLOAT, 40, PA]).op("OpStore", &[PA, ONE]).op("OpStore", &[PB, 40]);
    a.op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let t = text(&a);
    before(&t, "float _40 = a;", "a = 1.0;");
    before(&t, "a = 1.0;", "b = _40;");
}

#[test]
fn a_value_used_after_its_block_is_hoisted_out_of_the_braces() {
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpBranch", &[31]);
    a.op("OpLabel", &[31]).op("OpLoopMerge", &[34, 33, 0]).op("OpBranch", &[32]);
    a.op("OpLabel", &[32]).op("OpFAdd", &[FLOAT, 40, ONE, TWO]).op("OpBranchConditional", &[YES, 34, 33]);
    a.op("OpLabel", &[33]).op("OpBranch", &[31]);
    a.op("OpLabel", &[34]).op("OpStore", &[PA, 40]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let t = text(&a);
    before(&t, "float _40;", "while (true) {");
    assert!(t.contains("_40 = 1.0 + 2.0;") && t.contains("a = _40;"), "{}", t);
}

#[test]
fn wave_operations_have_their_hlsl_names() {
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpGroupNonUniformElect", &[BOOL, 40, ONE]).op("OpSelectionMerge", &[32, 0]).op("OpBranchConditional", &[40, 31, 32]);
    a.op("OpLabel", &[31]).op("OpGroupNonUniformFAdd", &[FLOAT, 41, ONE, 0, TWO]).op("OpGroupNonUniformFAdd", &[FLOAT, 42, ONE, 2, 41]).op("OpStore", &[PA, 42]).op("OpBranch", &[32]);
    a.op("OpLabel", &[32]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let t = text(&a);
    assert!(t.contains("if (WaveIsFirstLane()) {"), "{}", t);
    assert!(t.contains("WavePrefixSum(WaveActiveSum(2.0))"), "{}", t);
}

#[test]
fn unknown_instructions_stay_visible() {
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpGroupNonUniformQuadSwap", &[BOOL, 40, 3, YES, 0]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let s = decode(&a.bytes()).unwrap();
    assert!(s.hlsl.contains("OpGroupNonUniformQuadSwap"), "{}", s.hlsl);
    assert_eq!(s.warnings.len(), 1);
}

#[test]
fn scope_checker_finds_a_name_used_outside_its_braces() {
    assert_eq!(scope_errors("void main() {\n    if (true) {\n        float _7 = 1.0;\n    }\n    a = _7;\n}\n"), ["_7"]);
    assert!(scope_errors("void main() {\n    float _7 = 1.0;\n    if (true) {\n        a = _7;\n    }\n}\n").is_empty());
    // an unnamed struct is declared by its `struct` line and used as a member type
    assert!(scope_errors("struct _9 {\n    float a; // offset 0\n};\n\ncbuffer b {\n    _9 c; // offset 0\n}\n").is_empty());
}

#[test]
fn damaged_modules_never_panic() {
    assert!(decode(b"").is_err());
    assert!(decode(&[0u8; 64]).is_err());
    let mut a = prelude();
    a.op("OpLabel", &[30]).op("OpSelectionMerge", &[33, 0]).op("OpBranchConditional", &[YES, 31, 32]);
    a.op("OpLabel", &[31]).op("OpLoad", &[FLOAT, 41, PA]).op("OpBranch", &[33]);
    a.op("OpLabel", &[32]).op("OpImageWrite", &[PA, ONE, TWO]).op("OpBranch", &[33]);
    a.op("OpLabel", &[33]).op("OpPhi", &[FLOAT, 40, ONE, 31, TWO, 32]).op("OpStore", &[PA, 40]).op("OpReturn", &[]).op("OpFunctionEnd", &[]);
    let good = a.bytes();
    // a fixed xorshift: every word is hit several times with ids, opcodes and lengths gone wrong
    let mut x = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..30_000 {
        let mut d = good.clone();
        for _ in 0..3 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let at = 20 + (x as usize % (d.len() - 20));
            d[at] = (x >> 32) as u8 & if x & 256 != 0 { 0x3f } else { 0xff };
        }
        let _ = decode(&d);
    }
}

#[test]
fn a_type_that_names_itself_is_harmless() {
    let mut a = Asm::default();
    a.op("OpTypeVector", &[5, 5, 4]).op("OpTypeArray", &[6, 6, 7]).op("OpTypeStruct", &[8, 8]).op("OpConstantComposite", &[5, 9, 9]);
    a.op("OpTypePointer", &[10, 6, 8]).op("OpVariable", &[10, 11, 6]);
    assert!(decode(&a.bytes()).is_ok());
}
