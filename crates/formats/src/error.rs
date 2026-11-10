//! Error type and bounds-checked little-endian readers shared by the format parsers.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The file is not #ivo geometry.
    NotIvo(String),
    Msg(String),
}

impl Error {
    pub fn msg(s: impl Into<String>) -> Error {
        Error::Msg(s.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotIvo(s) | Error::Msg(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn take<const N: usize>(b: &[u8], o: usize) -> Result<[u8; N]> {
    o.checked_add(N)
        .and_then(|e| b.get(o..e))
        .map(|s| s.try_into().unwrap())
        .ok_or_else(|| Error::Msg(format!("unpack_from requires a buffer of at least {} bytes (offset {}, buffer {})", o.saturating_add(N), o, b.len())))
}

pub fn u8_at(b: &[u8], o: usize) -> Result<u8> {
    Ok(take::<1>(b, o)?[0])
}
pub fn u16_at(b: &[u8], o: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(take(b, o)?))
}
pub fn u32_at(b: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(take(b, o)?))
}
pub fn u64_at(b: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(take(b, o)?))
}
pub fn i16_at(b: &[u8], o: usize) -> Result<i16> {
    Ok(i16::from_le_bytes(take(b, o)?))
}
pub fn i64_at(b: &[u8], o: usize) -> Result<i64> {
    Ok(i64::from_le_bytes(take(b, o)?))
}
/// float32 widened to f64.
pub fn f32_at(b: &[u8], o: usize) -> Result<f64> {
    Ok(f32::from_le_bytes(take(b, o)?) as f64)
}
pub fn f64_at(b: &[u8], o: usize) -> Result<f64> {
    Ok(f64::from_le_bytes(take(b, o)?))
}
pub fn f32s<const N: usize>(b: &[u8], o: usize) -> Result<[f64; N]> {
    let mut out = [0.0; N];
    for (i, v) in out.iter_mut().enumerate() {
        *v = f32_at(b, o + 4 * i)?;
    }
    Ok(out)
}

/// IEEE half float to f64 (exact).
pub fn half(h: u16) -> f64 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1F) as i32;
    let m = (h & 0x3FF) as f64;
    match e {
        0 => s * m * 2f64.powi(-24),
        31 => {
            if m == 0.0 {
                s * f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => s * (1.0 + m / 1024.0) * 2f64.powi(e - 15),
    }
}

/// Python's `round()` on a float: ties go to the even integer.
pub fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}
