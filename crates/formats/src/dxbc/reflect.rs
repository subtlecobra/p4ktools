//! RDEF (resource definitions: constant buffers, variables, types, bound resources) and the signature chunks
//! (ISGN/ISG1, OSGN/OSG1/OSG5, PCSG/PSG1).
//!
//! Layouts follow the public/Wine reflection layout; EMPIRICAL on the game's shader cache (SM5 files carry the `RD11` block, so
//! resource bindings are 32 bytes, variables 40, types 36, type members 12, constant buffer descriptions 24).

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o.checked_add(4)?).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    d.get(o..o.checked_add(2)?).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn cstr(d: &[u8], o: usize) -> String {
    match d.get(o..) {
        Some(s) => {
            let e = s.iter().position(|&c| c == 0).unwrap_or(s.len().min(256));
            String::from_utf8_lossy(&s[..e.min(256)]).into_owned()
        }
        None => String::new(),
    }
}

#[derive(Clone, Debug, Default)]
pub struct RType {
    /// D3D_SHADER_VARIABLE_CLASS: 0 scalar, 1 vector, 2 matrix rows (row-major), 3 matrix columns, 4 object, 5 struct.
    pub class: u16,
    /// D3D_SHADER_VARIABLE_TYPE (1 bool, 2 int, 3 float, 19 uint, 39 double ...).
    pub vtype: u16,
    pub rows: u16,
    pub cols: u16,
    /// Array length (0 = not an array).
    pub elements: u16,
    pub members: Vec<(String, RType, u32)>,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct CbVar {
    pub name: String,
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
    pub ty: RType,
}

#[derive(Clone, Debug, Default)]
pub struct CBuffer {
    pub name: String,
    pub size: u32,
    /// 0 cbuffer, 1 tbuffer, 2 interface pointers, 3 resource bind info.
    pub kind: u32,
    pub vars: Vec<CbVar>,
}

#[derive(Clone, Debug, Default)]
pub struct ResBinding {
    pub name: String,
    /// D3D_SHADER_INPUT_TYPE: 0 cbuffer, 1 tbuffer, 2 texture, 3 sampler, 4 uav rw typed, 5 structured, 6 uav rw structured,
    /// 7 byteaddress, 8 uav rw byteaddress, 9 uav append, 10 uav consume, 11 uav rw structured with counter.
    pub kind: u32,
    /// D3D_RESOURCE_RETURN_TYPE: 1 unorm, 2 snorm, 3 sint, 4 uint, 5 float, 6 mixed, 7 double, 8 continued.
    pub ret: u32,
    /// D3D_SRV_DIMENSION: 1 buffer, 2 1d, 3 1darray, 4 2d, 5 2darray, 6 2dms, 7 2dmsarray, 8 3d, 9 cube, 10 cubearray, 11 bufferex.
    pub dim: u32,
    pub samples: u32,
    pub bind_point: u32,
    pub bind_count: u32,
    pub flags: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Rdef {
    pub major: u8,
    pub minor: u8,
    pub creator: String,
    pub cbuffers: Vec<CBuffer>,
    pub resources: Vec<ResBinding>,
}

fn parse_type(d: &[u8], off: usize, sm5: bool, depth: u32, budget: &mut u32) -> Option<RType> {
    if depth > 12 || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let mut t = RType {
        class: u16_at(d, off)?,
        vtype: u16_at(d, off + 2)?,
        rows: u16_at(d, off + 4)?,
        cols: u16_at(d, off + 6)?,
        elements: u16_at(d, off + 8)?,
        ..Default::default()
    };
    let nm = u16_at(d, off + 10)? as usize;
    let moff = u32_at(d, off + 12)? as usize;
    if sm5 {
        // EMPIRICAL: the SM5 type record is 36 bytes; the last dword is the type name offset (4 dwords before it unused here).
        if let Some(no) = u32_at(d, off + 32) {
            if no != 0 {
                t.name = cstr(d, no as usize);
            }
        }
    }
    if nm > 0 && nm < 4096 && moff != 0 {
        for i in 0..nm {
            let mo = moff + i * 12;
            let name = cstr(d, u32_at(d, mo)? as usize);
            let to = u32_at(d, mo + 4)? as usize;
            let boff = u32_at(d, mo + 8)?;
            let ty = parse_type(d, to, sm5, depth + 1, budget)?;
            t.members.push((name, ty, boff));
        }
    }
    Some(t)
}

pub fn parse_rdef(d: &[u8]) -> Result<Rdef, String> {
    let ncb = u32_at(d, 0).ok_or("RDEF too short")? as usize;
    let cbo = u32_at(d, 4).ok_or("RDEF too short")? as usize;
    let nres = u32_at(d, 8).ok_or("RDEF too short")? as usize;
    let reso = u32_at(d, 12).ok_or("RDEF too short")? as usize;
    let ver = u32_at(d, 16).ok_or("RDEF too short")?;
    let creator_off = u32_at(d, 24).ok_or("RDEF too short")? as usize;
    let major = ((ver >> 8) & 0xff) as u8;
    let minor = (ver & 0xff) as u8;
    let sm5 = major >= 5;
    if ncb > 4096 || nres > 4096 {
        return Err("RDEF counts implausible".into());
    }
    let mut r = Rdef { major, minor, creator: cstr(d, creator_off), ..Default::default() };
    let mut budget = 20000u32;
    for i in 0..nres {
        let o = reso + i * 32;
        r.resources.push(ResBinding {
            name: cstr(d, u32_at(d, o).ok_or("RDEF resource table truncated")? as usize),
            kind: u32_at(d, o + 4).ok_or("trunc")?,
            ret: u32_at(d, o + 8).ok_or("trunc")?,
            dim: u32_at(d, o + 12).ok_or("trunc")?,
            samples: u32_at(d, o + 16).ok_or("trunc")?,
            bind_point: u32_at(d, o + 20).ok_or("trunc")?,
            bind_count: u32_at(d, o + 24).ok_or("trunc")?,
            flags: u32_at(d, o + 28).ok_or("trunc")?,
        });
    }
    let vsize = if sm5 { 40 } else { 24 };
    for i in 0..ncb {
        let o = cbo + i * 24;
        let nv = u32_at(d, o + 4).ok_or("RDEF cbuffer table truncated")? as usize;
        let vo = u32_at(d, o + 8).ok_or("trunc")? as usize;
        let mut cb = CBuffer {
            name: cstr(d, u32_at(d, o).ok_or("trunc")? as usize),
            size: u32_at(d, o + 12).ok_or("trunc")?,
            kind: u32_at(d, o + 20).ok_or("trunc")?,
            vars: vec![],
        };
        if nv > 65536 {
            return Err("RDEF variable count implausible".into());
        }
        for k in 0..nv {
            let p = vo + k * vsize;
            let to = u32_at(d, p + 16).ok_or("RDEF variable table truncated")? as usize;
            let ty = parse_type(d, to, sm5, 0, &mut budget).unwrap_or_default();
            cb.vars.push(CbVar {
                name: cstr(d, u32_at(d, p).ok_or("trunc")? as usize),
                offset: u32_at(d, p + 4).ok_or("trunc")?,
                size: u32_at(d, p + 8).ok_or("trunc")?,
                flags: u32_at(d, p + 12).ok_or("trunc")?,
                ty,
            });
        }
        r.cbuffers.push(cb);
    }
    Ok(r)
}

#[derive(Clone, Debug, Default)]
pub struct SigElem {
    pub name: String,
    pub index: u32,
    /// D3D_NAME system value (0 = none).
    pub sysval: u32,
    /// 1 uint, 2 sint, 3 float.
    pub ctype: u32,
    /// Register number (0xffffffff for depth/coverage style outputs).
    pub reg: u32,
    pub mask: u8,
    pub rw: u8,
    pub stream: u32,
}

/// Parse a signature chunk. `tag` selects the entry layout: ISGN/OSGN/PCSG 24 bytes, ISG1/OSG1/PSG1 32 (stream first + min
/// precision last), OSG5 28 (stream first).
pub fn parse_sig(tag: &[u8], d: &[u8]) -> Result<Vec<SigElem>, String> {
    let n = u32_at(d, 0).ok_or("signature too short")? as usize;
    let (size, has_stream) = match tag {
        b"ISG1" | b"OSG1" | b"PSG1" => (32usize, true),
        b"OSG5" => (28, true),
        _ => (24, false),
    };
    if n > 1024 {
        return Err("signature count implausible".into());
    }
    let mut v = Vec::new();
    for i in 0..n {
        let mut o = 8 + i * size;
        let mut e = SigElem::default();
        if has_stream {
            e.stream = u32_at(d, o).ok_or("signature truncated")?;
            o += 4;
        }
        e.name = cstr(d, u32_at(d, o).ok_or("signature truncated")? as usize);
        e.index = u32_at(d, o + 4).ok_or("trunc")?;
        e.sysval = u32_at(d, o + 8).ok_or("trunc")?;
        e.ctype = u32_at(d, o + 12).ok_or("trunc")?;
        e.reg = u32_at(d, o + 16).ok_or("trunc")?;
        let m = u32_at(d, o + 20).ok_or("trunc")?;
        e.mask = (m & 0xff) as u8;
        e.rw = ((m >> 8) & 0xff) as u8;
        v.push(e);
    }
    Ok(v)
}

/// Size in bytes of one (non-array) instance of the type, with HLSL constant buffer packing (each register is 16 bytes).
pub fn elem_size(t: &RType) -> u32 {
    match t.class {
        0 | 1 => t.rows.max(1) as u32 * t.cols.max(1) as u32 * 4 * if t.class == 1 { 1 } else { 1 },
        2 => (t.rows.max(1) as u32 - 1) * 16 + t.cols.max(1) as u32 * 4,
        3 => (t.cols.max(1) as u32 - 1) * 16 + t.rows.max(1) as u32 * 4,
        5 => t.members.iter().map(|(_, m, o)| o + type_size(m)).max().unwrap_or(0),
        _ => 4,
    }
}

pub fn type_size(t: &RType) -> u32 {
    let e = elem_size(t);
    if t.elements > 1 {
        (t.elements as u32 - 1) * ((e + 15) & !15) + e
    } else {
        e
    }
}

/// One cbuffer component resolved to an HLSL path.
#[derive(Clone, Debug)]
pub struct CbComp {
    /// Path of the scalar/vector/matrix object, e.g. `g_Params[2].color`.
    pub base: String,
    /// Component name relative to the base: "x".."w" for vectors, "_m01" style for matrices, "" for scalars.
    pub comp: String,
    pub vtype: u16,
    pub is_matrix: bool,
    /// Components of the base object: 1 scalar, N vector, 0 matrix.
    pub width: u8,
}

fn resolve_in_type(t: &RType, name: &str, off: u32, depth: u32) -> Option<CbComp> {
    if depth > 12 {
        return None;
    }
    if t.elements > 1 {
        let es = elem_size(t);
        let stride = (es + 15) & !15;
        let idx = off / stride;
        if idx >= t.elements as u32 {
            return None;
        }
        let mut inner = t.clone();
        inner.elements = 0;
        return resolve_in_type(&inner, &format!("{}[{}]", name, idx), off % stride, depth + 1);
    }
    match t.class {
        5 => {
            for (n, m, mo) in &t.members {
                if off >= *mo && off < mo + type_size(m).max(4) {
                    return resolve_in_type(m, &format!("{}.{}", name, n), off - mo, depth + 1);
                }
            }
            None
        }
        0 | 1 => {
            let c = off / 4;
            if c >= t.cols.max(1) as u32 * t.rows.max(1) as u32 {
                return None;
            }
            let comp = if t.class == 0 { String::new() } else { ["x", "y", "z", "w"].get(c as usize)?.to_string() };
            Some(CbComp { base: name.to_string(), comp, vtype: t.vtype, is_matrix: false, width: if t.class == 0 { 1 } else { t.cols as u8 } })
        }
        2 | 3 => {
            let reg = off / 16;
            let c = (off % 16) / 4;
            let (row, col) = if t.class == 2 { (reg, c) } else { (c, reg) };
            if row >= t.rows as u32 || col >= t.cols as u32 {
                return None;
            }
            Some(CbComp { base: name.to_string(), comp: format!("_m{}{}", row, col), vtype: t.vtype, is_matrix: true, width: 0 })
        }
        _ => None,
    }
}

impl CBuffer {
    /// Resolve register `reg` component `comp` (0..3) to a variable path.
    pub fn resolve(&self, reg: u32, comp: u32) -> Option<CbComp> {
        let byte = reg.checked_mul(16)?.checked_add(comp * 4)?;
        for v in &self.vars {
            let sz = type_size(&v.ty).max(v.size).max(4);
            if byte >= v.offset && byte < v.offset + sz {
                return resolve_in_type(&v.ty, &v.name, byte - v.offset, 0);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_t(cols: u16) -> RType {
        RType { class: 1, vtype: 3, rows: 1, cols, ..Default::default() }
    }

    #[test]
    fn resolve_packed_vars_and_arrays() {
        let cb = CBuffer {
            name: "c".into(),
            size: 64,
            kind: 0,
            vars: vec![
                CbVar { name: "a".into(), offset: 0, size: 12, flags: 0, ty: vec_t(3) },
                CbVar { name: "s".into(), offset: 12, size: 4, flags: 0, ty: RType { class: 0, vtype: 3, rows: 1, cols: 1, ..Default::default() } },
                CbVar { name: "arr".into(), offset: 16, size: 32, flags: 0, ty: RType { elements: 2, ..vec_t(4) } },
                CbVar {
                    name: "m".into(),
                    offset: 48,
                    size: 64,
                    flags: 0,
                    ty: RType { class: 3, vtype: 3, rows: 4, cols: 4, ..Default::default() },
                },
            ],
        };
        let r = cb.resolve(0, 2).unwrap();
        assert_eq!((r.base.as_str(), r.comp.as_str()), ("a", "z"));
        assert_eq!(cb.resolve(0, 3).unwrap().base, "s");
        assert_eq!(cb.resolve(2, 1).unwrap().base, "arr[1]");
        let m = cb.resolve(3, 2).unwrap();
        assert_eq!((m.base.as_str(), m.comp.as_str()), ("m", "_m20"));
        assert!(cb.resolve(40, 0).is_none());
    }
}
