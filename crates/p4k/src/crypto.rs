//! Decryption of the archive's encrypted entries (the key is read from the user's own game executable and is not stored in this
//! repository).
//!
//! Facts (CONFIRMED against the engine, and checked on AEGS_Idris.xml and 150 random flagged entries):
//! * AES-128-CBC with one global key, IV = 16 zero bytes at the start of every entry, whole 16-byte blocks; then the usual
//!   decompression (zstd for method 100).  The stored size is a multiple of 16 and zero bytes follow the end of the zstd frame.
//! * The entry flag is NOT the zip flag bit 0 (always 0): each central-directory record has a 206-byte extra block, and the u16 at
//!   extra + 168 (tag 0x5002) is 1 for encrypted entries (4,276 of 1,379,308 in the live archive, all method 100 under Data/Scripts).
//! * The key sits in the executable as two 8-byte halves 0x84 bytes apart.  The second half follows the byte pattern
//!   CA FE BE DE DA CE BE FA BE AF AB AD and a run of zero bytes (40 in the live build); addresses differ between builds, so the
//!   key is found by that structure.

use std::path::{Path, PathBuf};

pub const EXTRA_LEN: usize = 206;
pub const FLAG_OFFSET: usize = 168;
pub const MARK: [u8; 12] = [0xca, 0xfe, 0xbe, 0xde, 0xda, 0xce, 0xbe, 0xfa, 0xbe, 0xaf, 0xab, 0xad];
pub const HALF_GAP: usize = 0x84;

/// `$P4K_EXE`, or `Bin64/StarCitizen.exe` next to the archive.
pub fn exe_path(p4k_path: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("P4K_EXE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let abs = std::fs::canonicalize(p4k_path).unwrap_or_else(|_| p4k_path.to_path_buf());
    abs.parent().unwrap_or(Path::new("")).join("Bin64").join("StarCitizen.exe")
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    // scan for the first byte, then compare: much quicker than comparing every window on a several-hundred-MB executable
    let mut i = from;
    while i + needle.len() <= hay.len() {
        match hay[i..hay.len() - needle.len() + 1].iter().position(|&b| b == needle[0]) {
            None => return None,
            Some(p) => {
                i += p;
                if &hay[i..i + needle.len()] == needle {
                    return Some(i);
                }
                i += 1;
            }
        }
    }
    None
}

/// The 16-byte archive key from the game executable (located by structure), or an error message.
pub fn find_key(exe: &Path) -> Result<[u8; 16], String> {
    let f = std::fs::File::open(exe).map_err(|e| format!("cannot read {}: {}", exe.display(), e))?;
    let m = unsafe { memmap2::Mmap::map(&f) }.map_err(|e| format!("cannot read {}: {}", exe.display(), e))?;
    let mut at = find(&m, &MARK, 0);
    while let Some(a) = at {
        let mut half2 = a + MARK.len();
        // a run of zero bytes (40 in the live build), then the second half
        while half2 < m.len() && m[half2] == 0 {
            half2 += 1;
        }
        if half2 - a - MARK.len() >= 16 && half2 >= HALF_GAP && half2 + 8 <= m.len() {
            let mut key = [0u8; 16];
            key[..8].copy_from_slice(&m[half2 - HALF_GAP..half2 - HALF_GAP + 8]);
            key[8..].copy_from_slice(&m[half2..half2 + 8]);
            return Ok(key);
        }
        at = find(&m, &MARK, a + 1);
    }
    Err(format!("the archive key is not in {} (unsupported build?)", exe.display()))
}

/// AES-128-CBC with a zero IV over the whole 16-byte blocks of `data`.
pub fn decrypt(data: &[u8], key: &[u8; 16]) -> Vec<u8> {
    crate::aes::cbc_decrypt(data, key, [0; 16]).expect("16-byte key")
}
