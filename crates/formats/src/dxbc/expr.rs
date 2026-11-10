//! Typed expression trees for the decompiler: literals, register-read markers (so single-use temporaries can be folded
//! afterwards), bit casts, swizzles, comparison masks, and precedence-aware printing.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ty {
    F,
    I,
    U,
    /// Raw 32-bit pattern whose type is decided by the consumer (immediates only).
    Any,
}

impl Ty {
    pub fn scalar(self) -> &'static str {
        match self {
            Ty::F => "float",
            Ty::I => "int",
            Ty::U | Ty::Any => "uint",
        }
    }
    pub fn vec(self, w: u8) -> String {
        if w <= 1 {
            self.scalar().to_string()
        } else {
            format!("{}{}", self.scalar(), w)
        }
    }
    pub fn cast_fn(self) -> &'static str {
        match self {
            Ty::F => "asfloat",
            Ty::I => "asint",
            Ty::U | Ty::Any => "asuint",
        }
    }
}

/// A temporary register read: which register, which components (in operand order) and the type the reader wants.
#[derive(Clone, Debug)]
pub struct ReadInfo {
    pub reg: u32,
    pub comps: Vec<u8>,
    pub want: Ty,
}

pub struct RCtx<'a> {
    pub reads: &'a [ReadInfo],
    pub regty: &'a [Ty],
}

#[derive(Clone, Debug)]
pub enum Piece {
    S(String),
    E(Ex),
}

#[derive(Clone, Debug)]
pub enum Node {
    /// Raw bits, one per component.
    Lit(Vec<u32>),
    /// Read of a temporary register (index into the read table).
    Reg(usize),
    /// Preformatted atom.
    Raw(String),
    /// Atom with embedded expressions (relative indices).
    Fmt(Vec<Piece>),
    Call(String, Vec<Ex>),
    Bin(&'static str, u8, Box<Ex>, Box<Ex>),
    Un(&'static str, Box<Ex>),
    Cast(Ty, Box<Ex>),
    Cond(Box<Ex>, Box<Ex>, Box<Ex>),
    Swz(Box<Ex>, Vec<u8>),
    /// Comparison producing a 0 / 0xffffffff mask per component.
    Cmp(&'static str, Box<Ex>, Box<Ex>),
    Vecn(Vec<Ex>),
}

#[derive(Clone, Debug)]
pub struct Ex {
    pub n: Node,
    pub ty: Ty,
    pub w: u8,
}

impl Ex {
    pub fn new(n: Node, ty: Ty, w: u8) -> Ex {
        Ex { n, ty, w }
    }
    pub fn raw(s: impl Into<String>, ty: Ty, w: u8) -> Ex {
        Ex::new(Node::Raw(s.into()), ty, w)
    }
    pub fn call(name: &str, args: Vec<Ex>, ty: Ty, w: u8) -> Ex {
        Ex::new(Node::Call(name.to_string(), args), ty, w)
    }
    pub fn bin(op: &'static str, prec: u8, a: Ex, b: Ex, ty: Ty) -> Ex {
        let w = a.w.max(b.w);
        Ex::new(Node::Bin(op, prec, Box::new(a), Box::new(b)), ty, w)
    }
    pub fn un(op: &'static str, a: Ex, ty: Ty) -> Ex {
        let w = a.w;
        Ex::new(Node::Un(op, Box::new(a)), ty, w)
    }
}

pub fn lit(bits: Vec<u32>, ty: Ty) -> Ex {
    let w = bits.len() as u8;
    Ex::new(Node::Lit(bits), ty, w)
}

/// Reinterpret `e` as `want` (a bit cast, never a value conversion).
pub fn coerce(mut e: Ex, want: Ty) -> Ex {
    if want == Ty::Any || e.ty == want {
        return e;
    }
    if let Node::Lit(_) = e.n {
        e.ty = want;
        return e;
    }
    if e.ty == Ty::Any {
        e.ty = want;
        return e;
    }
    // a bit cast of a bit cast is a single bit cast of the innermost value
    if let Node::Cast(_, inner) = e.n {
        return coerce(*inner, want);
    }
    let w = e.w;
    Ex::new(Node::Cast(want, Box::new(e)), want, w)
}

/// Component selection by position.
pub fn swz(e: Ex, pos: &[u8]) -> Ex {
    if pos.is_empty() {
        return e;
    }
    if e.w <= 1 {
        if pos.len() == 1 {
            return e;
        }
        if let Node::Lit(b) = &e.n {
            let v = vec![b[0]; pos.len()];
            return Ex::new(Node::Lit(v), e.ty, pos.len() as u8);
        }
        let ty = e.ty;
        return Ex::new(Node::Swz(Box::new(e), vec![0; pos.len()]), ty, pos.len() as u8);
    }
    if pos.len() == e.w as usize && pos.iter().enumerate().all(|(i, &p)| p as usize == i) {
        return e;
    }
    let ty = e.ty;
    match e.n {
        Node::Lit(b) => {
            let v: Vec<u32> = pos.iter().map(|&p| b.get(p as usize).copied().unwrap_or(0)).collect();
            Ex::new(Node::Lit(v), ty, pos.len() as u8)
        }
        Node::Swz(inner, p0) => {
            let np: Vec<u8> = pos.iter().map(|&p| p0.get(p as usize).copied().unwrap_or(0)).collect();
            swz(*inner, &np)
        }
        n => Ex::new(Node::Swz(Box::new(Ex { n, ty, w: e.w }), pos.to_vec()), ty, pos.len() as u8),
    }
}

const L: [char; 4] = ['x', 'y', 'z', 'w'];

fn fmt_float_lit(v: u32) -> String {
    let f = f32::from_bits(v);
    if !f.is_finite() {
        return format!("asfloat(0x{:08x})", v);
    }
    if f == 0.0 {
        return if v >> 31 != 0 { "-0.0".into() } else { "0.0".into() };
    }
    let a = f.abs();
    if a >= 1e9 || a < 1e-4 {
        format!("{:e}", f)
    } else if f.fract() == 0.0 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

fn lit_scalar(v: u32, ty: Ty) -> String {
    match ty {
        Ty::F => fmt_float_lit(v),
        Ty::I => format!("{}", v as i32),
        Ty::U => {
            if v < 65536 {
                format!("{}u", v)
            } else {
                format!("0x{:x}u", v)
            }
        }
        Ty::Any => {
            let e = (v >> 23) & 0xff;
            if v == 0 {
                "0".into()
            } else if (0x40..0xc0).contains(&e) {
                fmt_float_lit(v)
            } else if (v as i32).unsigned_abs() < 65536 {
                format!("{}", v as i32)
            } else {
                format!("0x{:x}", v)
            }
        }
    }
}

pub struct Printer<'a> {
    pub c: &'a RCtx<'a>,
}

impl<'a> Printer<'a> {
    /// Render with parentheses when the expression binds looser than `min`.
    pub fn min(&self, e: &Ex, min: u8) -> String {
        let (s, p) = self.render(e);
        if p < min {
            format!("({})", s)
        } else {
            s
        }
    }

    pub fn render(&self, e: &Ex) -> (String, u8) {
        match &e.n {
            Node::Lit(b) => {
                let ty = e.ty;
                if b.len() == 1 {
                    let s = lit_scalar(b[0], ty);
                    let p = if s.starts_with('-') { 90 } else { 100 };
                    (s, p)
                } else if b.iter().all(|&x| x == b[0]) {
                    (format!("({}){}", ty.vec(b.len() as u8), lit_scalar(b[0], ty)), 90)
                } else {
                    let v: Vec<String> = b.iter().map(|&x| lit_scalar(x, ty)).collect();
                    (format!("{}({})", ty.vec(b.len() as u8), v.join(", ")), 100)
                }
            }
            Node::Reg(id) => {
                let Some(r) = self.c.reads.get(*id) else { return ("<?>".into(), 100) };
                let regty = self.c.regty.get(r.reg as usize).copied().unwrap_or(Ty::F);
                let mut s = format!("r{}", r.reg);
                let full = r.comps.len() == 4 && r.comps.iter().enumerate().all(|(i, &c)| c as usize == i);
                if !full {
                    s.push('.');
                    for &c in &r.comps {
                        s.push(L[c as usize & 3]);
                    }
                }
                if r.want != Ty::Any && r.want != regty {
                    (format!("{}({})", r.want.cast_fn(), s), 100)
                } else {
                    (s, 100)
                }
            }
            Node::Raw(s) => (s.clone(), 100),
            Node::Fmt(ps) => {
                let mut s = String::new();
                for p in ps {
                    match p {
                        Piece::S(t) => s.push_str(t),
                        Piece::E(x) => s.push_str(&self.min(x, 0)),
                    }
                }
                (s, 100)
            }
            Node::Call(n, a) => {
                let v: Vec<String> = a.iter().map(|x| self.min(x, 0)).collect();
                (format!("{}({})", n, v.join(", ")), 100)
            }
            Node::Bin("+", p, a, b) if matches!(&b.n, Node::Un("-", _)) => {
                let Node::Un(_, x) = &b.n else { unreachable!() };
                (format!("{} - {}", self.min(a, *p), self.min(x, *p + 1)), *p)
            }
            Node::Bin(op, p, a, b) => {
                let (l, r) = if *p == 0 { (0, 0) } else { (*p, *p + 1) };
                (format!("{} {} {}", self.min(a, l), op, self.min(b, r)), *p)
            }
            Node::Un(op, a) => (format!("{}{}", op, self.min(a, 91)), 90),
            Node::Cast(t, a) => (format!("{}({})", t.cast_fn(), self.min(a, 0)), 100),
            Node::Cond(c, a, b) => {
                let (cs, cp) = self.bool_of(c);
                let cs = if cp < 11 { format!("({})", cs) } else { cs };
                (format!("{} ? {} : {}", cs, self.min(a, 11), self.min(b, 10)), 10)
            }
            Node::Swz(a, pos) => {
                let mut s = self.min(a, 100);
                s.push('.');
                for &p in pos {
                    s.push(L[p as usize & 3]);
                }
                (s, 100)
            }
            Node::Cmp(op, a, b) => {
                let p = if *op == "==" || *op == "!=" { 46 } else { 51 };
                (format!("({} {} {}) ? 0xffffffffu : 0u", self.min(a, p), op, self.min(b, p)), 10)
            }
            Node::Vecn(v) => {
                let items: Vec<String> = v.iter().map(|x| self.min(x, 0)).collect();
                (format!("{}({})", e.ty.vec(e.w), items.join(", ")), 100)
            }
        }
    }

    /// Negated boolean rendering (`if_z`, `!cond`).
    pub fn not_of(&self, e: &Ex) -> (String, u8) {
        match &e.n {
            Node::Cmp(op, a, b) if a.ty != Ty::F => {
                let inv = match *op {
                    "<" => ">=",
                    ">=" => "<",
                    "==" => "!=",
                    "!=" => "==",
                    ">" => "<=",
                    _ => "==",
                };
                let p = if inv == "==" || inv == "!=" { 45 } else { 50 };
                (format!("{} {} {}", self.min(a, p + 1), inv, self.min(b, p + 1)), p)
            }
            Node::Cmp(..) | Node::Bin(..) | Node::Un("~", _) | Node::Lit(_) if is_mask(e) => {
                let (s, p) = self.bool_of(e);
                if let Node::Lit(_) = e.n {
                    return (if s == "true" { "false".into() } else { "true".into() }, 100);
                }
                (if p < 91 { format!("!({})", s) } else { format!("!{}", s) }, 90)
            }
            Node::Swz(x, _) if x.w == 1 && is_mask(x) => self.not_of(x),
            _ => {
                let e2 = if e.ty == Ty::F || e.ty == Ty::I { coerce(e.clone(), Ty::U) } else { e.clone() };
                (format!("{} == 0", self.min(&e2, 46)), 45)
            }
        }
    }

    /// Render in a boolean context (`if`, `?:` condition, `discard`).
    pub fn bool_of(&self, e: &Ex) -> (String, u8) {
        match &e.n {
            Node::Cmp(op, a, b) => {
                let p = if *op == "==" || *op == "!=" { 45 } else { 50 };
                (format!("{} {} {}", self.min(a, p + 1), op, self.min(b, p + 1)), p)
            }
            Node::Bin(op, _, a, b) if (*op == "&" || *op == "|") && is_mask(a) && is_mask(b) => {
                let (o, p) = if *op == "&" { ("&&", 30) } else { ("||", 20) };
                let l = self.bool_of(a);
                let r = self.bool_of(b);
                let l = if l.1 < p { format!("({})", l.0) } else { l.0 };
                let r = if r.1 <= p { format!("({})", r.0) } else { r.0 };
                (format!("{} {} {}", l, o, r), p)
            }
            Node::Un("~", a) if is_mask(a) => {
                let (s, p) = self.bool_of(a);
                (if p < 91 { format!("!({})", s) } else { format!("!{}", s) }, 90)
            }
            Node::Lit(b) if b.len() == 1 => (if b[0] == 0 { "false".into() } else { "true".into() }, 100),
            // a scalar mask broadcast over a vector selects the whole vector
            Node::Swz(x, _) if x.w == 1 && is_mask(x) => self.bool_of(x),
            _ => {
                let e2 = if e.ty == Ty::F || e.ty == Ty::I { coerce(e.clone(), Ty::U) } else { e.clone() };
                (format!("{} != 0", self.min(&e2, 46)), 45)
            }
        }
    }
}

pub fn is_mask(e: &Ex) -> bool {
    match &e.n {
        Node::Cmp(..) => true,
        Node::Bin(op, _, a, b) if *op == "&" || *op == "|" => is_mask(a) && is_mask(b),
        Node::Un("~", a) => is_mask(a),
        Node::Lit(b) => b.len() == 1 && (b[0] == 0 || b[0] == u32::MAX),
        _ => false,
    }
}

/// Visit the ids of all register-read markers inside `e`.
pub fn walk_reads(e: &Ex, f: &mut dyn FnMut(usize)) {
    match &e.n {
        Node::Reg(id) => f(*id),
        Node::Lit(_) | Node::Raw(_) => {}
        Node::Fmt(ps) => {
            for p in ps {
                if let Piece::E(x) = p {
                    walk_reads(x, f);
                }
            }
        }
        Node::Call(_, a) | Node::Vecn(a) => a.iter().for_each(|x| walk_reads(x, f)),
        Node::Bin(_, _, a, b) | Node::Cmp(_, a, b) => {
            walk_reads(a, f);
            walk_reads(b, f);
        }
        Node::Un(_, a) | Node::Cast(_, a) | Node::Swz(a, _) => walk_reads(a, f),
        Node::Cond(a, b, c) => {
            walk_reads(a, f);
            walk_reads(b, f);
            walk_reads(c, f);
        }
    }
}

/// Replace the marker `id` by `with` (taken); true when found.
pub fn replace_read(e: &mut Ex, id: usize, with: &mut Option<Ex>) -> bool {
    if let Node::Reg(r) = e.n {
        if r == id {
            if let Some(w) = with.take() {
                *e = w;
                return true;
            }
        }
        return false;
    }
    match &mut e.n {
        Node::Lit(_) | Node::Raw(_) | Node::Reg(_) => false,
        Node::Fmt(ps) => ps.iter_mut().any(|p| match p {
            Piece::E(x) => replace_read(x, id, with),
            _ => false,
        }),
        Node::Call(_, a) | Node::Vecn(a) => a.iter_mut().any(|x| replace_read(x, id, with)),
        Node::Bin(_, _, a, b) | Node::Cmp(_, a, b) => replace_read(a, id, with) || replace_read(b, id, with),
        Node::Un(_, a) | Node::Cast(_, a) | Node::Swz(a, _) => replace_read(a, id, with),
        Node::Cond(a, b, c) => replace_read(a, id, with) || replace_read(b, id, with) || replace_read(c, id, with),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr<'a>(c: &'a RCtx<'a>) -> Printer<'a> {
        Printer { c }
    }

    #[test]
    fn precedence_and_casts() {
        let reads = [ReadInfo { reg: 3, comps: vec![0, 1], want: Ty::F }];
        let regty = [Ty::F, Ty::F, Ty::F, Ty::I];
        let c = RCtx { reads: &reads, regty: &regty };
        let a = Ex::raw("a", Ty::F, 1);
        let b = Ex::raw("b", Ty::F, 1);
        let s = Ex::bin("+", 60, a.clone(), b.clone(), Ty::F);
        let m = Ex::bin("*", 70, s, Ex::raw("c", Ty::F, 1), Ty::F);
        assert_eq!(pr(&c).render(&m).0, "(a + b) * c");
        let m2 = Ex::bin("-", 60, a, Ex::bin("-", 60, b.clone(), b, Ty::F), Ty::F);
        assert_eq!(pr(&c).render(&m2).0, "a - (b - b)");
        let r = Ex::new(Node::Reg(0), Ty::F, 2);
        assert_eq!(pr(&c).render(&r).0, "asfloat(r3.xy)");
    }

    #[test]
    fn masks_become_booleans() {
        let reads: [ReadInfo; 0] = [];
        let regty: [Ty; 0] = [];
        let c = RCtx { reads: &reads, regty: &regty };
        let x = Ex::new(Node::Cmp("<", Box::new(Ex::raw("a", Ty::F, 1)), Box::new(Ex::raw("b", Ty::F, 1))), Ty::U, 1);
        let y = Ex::new(Node::Cmp("==", Box::new(Ex::raw("c", Ty::F, 1)), Box::new(Ex::raw("d", Ty::F, 1))), Ty::U, 1);
        let both = Ex::bin("&", 40, x.clone(), y, Ty::U);
        assert_eq!(pr(&c).bool_of(&both).0, "a < b && c == d");
        assert_eq!(pr(&c).bool_of(&Ex::un("~", x, Ty::U)).0, "!(a < b)");
        assert_eq!(pr(&c).bool_of(&Ex::raw("u", Ty::U, 1)).0, "u != 0");
    }

    #[test]
    fn literals() {
        let reads: [ReadInfo; 0] = [];
        let regty: [Ty; 0] = [];
        let c = RCtx { reads: &reads, regty: &regty };
        assert_eq!(pr(&c).render(&lit(vec![1.0f32.to_bits()], Ty::Any)).0, "1.0");
        assert_eq!(pr(&c).render(&lit(vec![5], Ty::U)).0, "5u");
        assert_eq!(pr(&c).render(&lit(vec![0xffff_ffff; 3], Ty::I)).0, "(int3)-1");
        let l = lit(vec![1.0f32.to_bits(), 2.0f32.to_bits(), 3.0f32.to_bits()], Ty::F);
        assert_eq!(pr(&c).render(&swz(l, &[2, 0])).0, "float2(3.0, 1.0)");
    }
}
