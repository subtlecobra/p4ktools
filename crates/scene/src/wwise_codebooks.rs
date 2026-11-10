//! Wwise Vorbis codebook library, read from the user's own game executable (nothing is bundled).
//!
//! CONFIRMED (byte comparison with ww2ogg's packed_codebooks_aoTuV_603.bin): the engine's 598 packed books are one 71,991-byte block
//! that starts with `SIGNATURE`, and a codebook pointer table holds 599 absolute 8-byte pointers into it (entry i = book i, entry 598 =
//! the end sentinel); the pointers are virtual addresses over `IMAGE_BASE`.  The addresses differ between builds, so both
//! are located by content: the signature, then the pointer table that begins (block, block + 8, block + 21).  Everything found is
//! validated; a mismatch means "unsupported build", not a guess.

use std::path::{Path, PathBuf};

const IMAGE_BASE: u64 = 0x1_4000_0000;
const BOOKS_SIZE: usize = 71991;
const NBOOKS: usize = 598;
const SIGNATURE: [u8; 16] = [0x91, 0x00, 0x58, 0x53, 0x55, 0x75, 0x75, 0x00, 0x91, 0x01, 0x58, 0x00, 0x75, 0x75, 0x95, 0xb5];

#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// (virtual address, size, raw offset) of every PE section.
fn sections(m: &[u8]) -> Option<Vec<(u64, u64, u64)>> {
    let pe = u32_at(m, 0x3C)? as usize;
    let n = u16_at(m, pe + 6)? as usize;
    let opt = u16_at(m, pe + 20)? as usize;
    let mut out = Vec::new();
    for i in 0..n {
        let s = pe + 24 + opt + 40 * i;
        let (vsize, va, rsize, raw) = (u32_at(m, s + 8)?, u32_at(m, s + 12)?, u32_at(m, s + 16)?, u32_at(m, s + 20)?);
        out.push((va as u64, vsize.max(rsize) as u64, raw as u64));
    }
    Some(out)
}

fn va_of(secs: &[(u64, u64, u64)], off: u64) -> Result<u64, Unavailable> {
    for &(va, size, raw) in secs {
        if raw <= off && off < raw + size {
            return Ok(IMAGE_BASE + va + off - raw);
        }
    }
    Err(Unavailable(format!("file offset {:#x} is not inside a section", off)))
}

/// The 598 packed books from the game executable.
pub fn load(exe_path: &Path) -> Result<Vec<Vec<u8>>, Unavailable> {
    let f = std::fs::File::open(exe_path).map_err(|e| Unavailable(format!("cannot read {}: {}", exe_path.display(), e)))?;
    let m = unsafe { memmap2::Mmap::map(&f) }.map_err(|e| Unavailable(format!("cannot read {}: {}", exe_path.display(), e)))?;
    let secs = sections(&m).ok_or_else(|| Unavailable(format!("cannot read {}: bad PE header", exe_path.display())))?;
    let at = memchr::memmem::find(&m, &SIGNATURE).ok_or_else(|| Unavailable(format!("the Vorbis codebook block is not in {}", exe_path.display())))?;
    let books = m.get(at..at + BOOKS_SIZE).ok_or_else(|| Unavailable("the codebook block is truncated".into()))?;
    let base = va_of(&secs, at as u64)?;
    let mut needle = Vec::with_capacity(24);
    for v in [base, base + 8, base + 21] {
        needle.extend_from_slice(&v.to_le_bytes());
    }
    let pt = memchr::memmem::find(&m, &needle).ok_or_else(|| Unavailable(format!("the codebook pointer table is not in {}", exe_path.display())))?;
    let tbl = m.get(pt..pt + 8 * (NBOOKS + 1)).ok_or_else(|| Unavailable("the codebook pointer table is truncated".into()))?;
    let offs: Vec<i64> = tbl.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap()).wrapping_sub(base) as i64).collect();
    if offs[0] != 0 || offs[NBOOKS] != BOOKS_SIZE as i64 || offs.windows(2).any(|w| w[1] <= w[0]) {
        return Err(Unavailable("the codebook pointer table does not match the book data".into()));
    }
    Ok(offs.windows(2).map(|w| books[w[0] as usize..w[1] as usize].to_vec()).collect())
}

/// `$P4K_EXE`, or Bin64/StarCitizen.exe next to the archive.
pub fn exe_path(p4k_path: &Path) -> PathBuf {
    match std::env::var_os("P4K_EXE") {
        Some(p) if !p.is_empty() => p.into(),
        _ => {
            let abs = std::fs::canonicalize(p4k_path).unwrap_or_else(|_| p4k_path.to_path_buf());
            abs.parent().map(Path::to_path_buf).unwrap_or_default().join("Bin64").join("StarCitizen.exe")
        }
    }
}
