//! Minimal PNG writer (8-bit RGBA, no filtering).  The deflate stream comes from flate2 (miniz_oxide) at level 6.

use std::io::Write;

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, slot) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn chunk(out: &mut Vec<u8>, t: &[u8; 4], d: &[u8]) {
    out.extend((d.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(t);
    out.extend(d);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

/// PNG bytes of a `w` x `h` RGBA image.
pub fn png_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let row = w as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * row..(y + 1) * row]);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    z.write_all(&raw).unwrap();
    let idat = z.finish().unwrap();
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", b"");
    out
}

/// Decode a PNG written by `png_rgba` (8-bit RGBA, filter 0 only): (w, h, pixels).  For round-trip tests.
pub fn decode_png_rgba(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    use std::io::Read;
    if png.len() < 8 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let (mut p, mut w, mut h, mut idat) = (8, 0u32, 0u32, Vec::new());
    while p + 8 <= png.len() {
        let len = u32::from_be_bytes(png[p..p + 4].try_into().ok()?) as usize;
        let t = &png[p + 4..p + 8];
        let d = png.get(p + 8..p + 8 + len)?;
        if t == b"IHDR" {
            w = u32::from_be_bytes(d[0..4].try_into().ok()?);
            h = u32::from_be_bytes(d[4..8].try_into().ok()?);
        } else if t == b"IDAT" {
            idat.extend_from_slice(d);
        }
        p += 12 + len;
    }
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&idat[..]).read_to_end(&mut raw).ok()?;
    let row = w as usize * 4;
    let mut out = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        let line = raw.get(y * (row + 1)..(y + 1) * (row + 1))?;
        if line[0] != 0 {
            return None;
        }
        out.extend_from_slice(&line[1..]);
    }
    Some((w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let px: Vec<u8> = (0..4 * 3 * 2).map(|i| (i * 7) as u8).collect();
        let png = png_rgba(3, 2, &px);
        assert_eq!(decode_png_rgba(&png), Some((3, 2, px)));
    }

    #[test]
    fn crc_of_iend() {
        // the well known CRC of an empty IEND chunk
        assert_eq!(crc32(b"IEND"), 0xAE426082);
    }
}
