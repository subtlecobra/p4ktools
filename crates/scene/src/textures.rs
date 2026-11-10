//! Reading DDS textures out of the archive, including the mip levels that are stored in side files.
//!
//! EMPIRICAL (checked on real assets, not against the engine loader): the base `<name>.dds` holds the header plus the smallest
//! mips (payload size equals the sum of the sizes of mips k..n-1); each larger mip i < k lives in its own file
//! `<name>.dds.(k - i)` holding exactly that mip's data.

use p4k_formats::dds::{self, Format};
use p4k::{Entry, P4k};

/// A texture path as written in a .mtl, without its extension.  EMPIRICAL: materials name '.tif' (almost always) but also '.dds'
/// (1,524 decals of asd_layout_facility_001, whose lookup used to become '....dds.dds'), and use either slash.
pub fn texture_base(path: &str) -> String {
    let p = path.replace('\\', "/");
    for ext in [".tif", ".tiff", ".dds"] {
        if p.to_lowercase().ends_with(ext) {
            return p[..p.len() - ext.len()].to_string();
        }
    }
    p
}

/// One archive .dds: header facts plus access to each mip level's raw bytes.  `new` fails for files that are not DDS, have an
/// unknown pixel format, or whose payload size does not match the mip chain.
pub struct DdsTexture<'a> {
    p4k: &'a P4k,
    ent_name: String,
    alpha: bool,
    pub width: usize,
    pub height: usize,
    pub mips: usize,
    pub dxgi: u32,
    pub format: Format,
    pub depth: usize,
    /// 6 for a cube map (face order +X -X +Y -Y +Z -Z), else 1; a cube's faces are served as the slices of each mip
    pub faces: usize,
    pub dims: Vec<(usize, usize)>,
    pub sizes: Vec<usize>,
    payload: Vec<u8>,
    pub first_stored: usize,
}

impl<'a> DdsTexture<'a> {
    /// alpha = true: `ent` is the '<name>.dds.a' file, the alpha / gloss companion of a '<name>.dds' (a BC4 texture with the same
    /// mip layout): its larger mips are in '<name>.dds.<n>a'.  EMPIRICAL (lowtech_metal_paint_worn_01_ddna: .dds.a holds a DDS
    /// header plus the smallest mips, .dds.1a ... .dds.7a hold one mip each at half the size of the BC5 mip, i.e. BC4).
    pub fn new(p4k: &'a P4k, ent: &Entry, alpha: bool) -> Result<Self, String> {
        let mut d = p4k.read(ent).map_err(|e| e.to_string())?;
        if alpha && d.get(..4) != Some(b"DDS ") {
            // EMPIRICAL: the .dds.a companion is stored without the 4-byte magic
            let mut v = b"DDS ".to_vec();
            v.extend_from_slice(&d);
            d = v;
        }
        let h = dds::parse_header(&d).ok_or("not a DDS file")?;
        let dxgi = h.dxgi.unwrap_or(0);
        let format = h.dxgi.and_then(dds::format_info).ok_or_else(|| format!("unsupported pixel format (DXGI {})", h.dxgi.map_or("None".into(), |v| v.to_string())))?;
        let depth = dds::parse_depth(&d);
        let faces = dds::parse_faces(&d);
        let (dims, mut sizes) = dds::mip_sizes(h.width, h.height, h.mips, &format, depth);
        // a cube map stores the faces of one mip one after another (every mip file / the base payload holds all faces of its mips)
        sizes.iter_mut().for_each(|s| *s *= faces);
        let payload = d.get(h.header_size..).unwrap_or(&[]).to_vec();
        let first_stored = (0..=h.mips)
            .find(|&i| sizes[i..].iter().sum::<usize>() == payload.len())
            .ok_or_else(|| format!("payload size {} does not match the mip chain", payload.len()))?;
        Ok(DdsTexture { p4k, ent_name: ent.name.to_string(), alpha, width: h.width, height: h.height, mips: h.mips, dxgi, format, depth, faces, dims, sizes, payload, first_stored })
    }

    fn side(&self, level: usize) -> Option<&'a Entry> {
        let n = self.first_stored - level;
        if self.alpha {
            let base = &self.ent_name[..self.ent_name.len() - 2];
            self.p4k.find_path(&format!("{}.{}a", base, n), &format!(".{}a", n))
        } else {
            self.p4k.find_path(&format!("{}.{}", self.ent_name, n), &format!(".{}", n))
        }
    }

    pub fn available(&self, level: usize) -> bool {
        level >= self.first_stored || self.side(level).is_some()
    }

    /// Depth slices in mip `level` (1 for an ordinary texture), or the faces of a cube map.
    pub fn slices(&self, level: usize) -> usize {
        if self.faces > 1 {
            return self.faces;
        }
        (self.depth >> level).max(1)
    }

    /// Raw bytes of slice k of mip `level` (a volume texture stores its slices one after another), or None.
    pub fn level_slice(&self, level: usize, k: usize) -> Option<Vec<u8>> {
        let data = self.level(level)?;
        let n = self.slices(level);
        if k >= n {
            return None;
        }
        let sz = self.sizes[level] / n;
        Some(data[k * sz..(k + 1) * sz].to_vec())
    }

    /// Raw bytes of mip `level` (0 = largest), or None when its side file is missing or unreadable.
    pub fn level(&self, level: usize) -> Option<Vec<u8>> {
        if level >= self.mips {
            return None;
        }
        let data: Vec<u8> = if level >= self.first_stored && self.faces > 1 {
            // EMPIRICAL (Monorail_Cubemap_cm.dds, the face-major payload decoded to the same picture as the side-file mip 3 above it, the
            // mip-major reading did not): the base file of a cube map holds, per face, that face's remaining mips one after another;
            // only the side files (one per mip) hold all six faces of a mip together.
            let n = self.faces;
            let stride: usize = self.sizes[self.first_stored..].iter().sum::<usize>() / n;
            let off: usize = self.sizes[self.first_stored..level].iter().sum::<usize>() / n;
            let one = self.sizes[level] / n;
            let mut v = Vec::with_capacity(self.sizes[level]);
            for f in 0..n {
                v.extend_from_slice(self.payload.get(f * stride + off..f * stride + off + one)?);
            }
            v
        } else if level >= self.first_stored {
            let off: usize = self.sizes[self.first_stored..level].iter().sum();
            self.payload.get(off..(off + self.sizes[level]).min(self.payload.len()))?.to_vec()
        } else {
            let side = self.side(level)?;
            self.p4k.read(side).ok()?
        };
        if data.len() >= self.sizes[level] {
            Some(data[..self.sizes[level]].to_vec())
        } else {
            None
        }
    }
}

/// The six faces of a cube map (each `w x h` RGBA, order +X -X +Y -Y +Z -Z) laid out as the usual horizontal cross, 4 faces wide and
/// 3 high: the top row has +Y in column 1, the middle row -X +Z +X -Z, the bottom row -Y in column 1.  The empty cells are opaque
/// black.  The layout assumes D3D face orientation (INFERRED, not checked against the engine); returns (width, height, RGBA).
pub fn cube_cross(w: usize, h: usize, faces: &[Vec<u8>]) -> (usize, usize, Vec<u8>) {
    let (cw, ch) = (w * 4, h * 3);
    let mut out = [0u8, 0, 0, 255].repeat(cw * ch);
    // (face index, column, row)
    for &(f, col, row) in &[(2usize, 1usize, 0usize), (1, 0, 1), (4, 1, 1), (0, 2, 1), (5, 3, 1), (3, 1, 2)] {
        let Some(src) = faces.get(f) else { continue };
        for y in 0..h {
            let d = ((row * h + y) * cw + col * w) * 4;
            out[d..d + w * 4].copy_from_slice(&src[y * w * 4..(y + 1) * w * 4]);
        }
    }
    (cw, ch, out)
}

/// RGBA of the largest mip with both sides <= `cap` that is available (bc1/2/3/4/5, rgba8, ... see `dds::DECODABLE`):
/// (w, h, pixels).  The same level choice as `ng_png` / `opacity_png`.  None for a format that is not decoded
/// or a missing mip.
pub fn decode_rgba(tex: &DdsTexture, cap: usize) -> Option<(usize, usize, Vec<u8>)> {
    if !dds::DECODABLE.contains(&tex.format.name) {
        return None;
    }
    let lvl = (0..tex.mips).find(|&i| tex.dims[i].0.max(tex.dims[i].1) <= cap && tex.available(i))?;
    let data = tex.level(lvl)?;
    let (w, h) = tex.dims[lvl];
    Some((w, h, dds::decode(tex.format.name, &data, w, h).ok()?))
}

// ---- texture loading for the material registry

use p4k_formats::png::png_rgba;

fn open_dds<'a>(p4k: &'a P4k, base: &str) -> Option<DdsTexture<'a>> {
    let ent = p4k.find_path(&format!("{}.dds", base), ".dds")?;
    DdsTexture::new(p4k, ent, false).ok()
}

/// A diffuse texture as the registry embeds it: `fmt` is "png" (decoded BC1/2/3, `srgb` false) or "bc7" (raw blocks for the
/// browser's BPTC extension).
pub struct LoadedTexture {
    pub fmt: &'static str,
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
    pub srgb: bool,
}

/// `tex_path` as written in the .mtl (.tif).  None when the file is missing, not BC1/2/3/7, or the chosen mip is not available.
/// The mip is the first (largest) whose longer side is <= `max_dim`, else the smallest one; unlike `decode_rgba` it does not skip
/// mips whose side file is missing.
pub fn load_texture(p4k: &P4k, tex_path: &str, max_dim: usize) -> Option<LoadedTexture> {
    let mut base = tex_path.replace('\\', "/");
    if base.to_lowercase().ends_with(".tif") {
        base.truncate(base.len() - 4);
    }
    let tex = open_dds(p4k, &base)?;
    let name = tex.format.name;
    if !matches!(name, "bc1" | "bc2" | "bc3" | "bc7") {
        return None;
    }
    let target = (0..tex.mips).find(|&i| tex.dims[i].0.max(tex.dims[i].1) <= max_dim).unwrap_or(tex.mips - 1);
    let data = tex.level(target)?;
    let (tw, th) = tex.dims[target];
    if name == "bc7" {
        return Some(LoadedTexture { fmt: "bc7", w: tw, h: th, data, srgb: tex.format.srgb });
    }
    Some(LoadedTexture { fmt: "png", w: tw, h: th, data: png_rgba(tw as u32, th as u32, &dds::decode_bc(name, &data, tw, th)), srgb: false })
}

/// (level, w, h, RGBA) of the largest mip of `tex` with both sides <= `cap` that is available; None for a format that is not decoded.
pub fn level_rgba(tex: &DdsTexture, cap: usize) -> Option<(usize, usize, usize, Vec<u8>)> {
    if !dds::DECODABLE.contains(&tex.format.name) {
        return None;
    }
    let lvl = (0..tex.mips).find(|&i| tex.dims[i].0.max(tex.dims[i].1) <= cap && tex.available(i))?;
    let data = tex.level(lvl)?;
    let (w, h) = tex.dims[lvl];
    Some((lvl, w, h, dds::decode(tex.format.name, &data, w, h).ok()?))
}

/// RGBA (w, h, pixels) of a ddna normal map and its gloss companion packed into one image: R, G = the decoded normal's x, y (z is
/// rebuilt by the shader), B = gloss (BC4 alpha companion '.dds.a', 128 when it is missing), A = 255.  The largest mip with both
/// sides <= cap.  None when the map is missing or in a format that is not decoded.
pub fn ng_rgba(p4k: &P4k, base: &str, cap: usize) -> Option<(usize, usize, Vec<u8>)> {
    let nt = open_dds(p4k, base)?;
    let gt = p4k.find_path(&format!("{}.dds.a", base), ".a").and_then(|g| DdsTexture::new(p4k, g, true).ok());
    let (lvl, w, h, mut out) = level_rgba(&nt, cap)?;
    let gloss = gt.as_ref().filter(|g| dds::DECODABLE.contains(&g.format.name) && lvl < g.mips && g.dims[lvl] == (w, h)).and_then(|g| {
        let gd = g.level(lvl)?;
        dds::decode(g.format.name, &gd, w, h).ok()
    });
    for i in 0..w * h {
        out[i * 4 + 2] = gloss.as_ref().map_or(128, |g| g[i * 4]);
        out[i * 4 + 3] = 255;
    }
    Some((w, h, out))
}

/// `ng_rgba` as (w, h, PNG bytes): shared by the zone / item texture servers and the embedded textures of the single-asset page.
pub fn ng_png(p4k: &P4k, base: &str, cap: usize) -> Option<(usize, usize, Vec<u8>)> {
    let (w, h, px) = ng_rgba(p4k, base, cap)?;
    Some((w, h, png_rgba(w as u32, h as u32, &px)))
}

/// RGBA of a HairPBR opacity map as an alpha-only image: RGB white, A = the map's R channel (HairPS reads OpacityMap.r, not its
/// alpha; CONFIRMED against the engine), so the viewers' plain alpha test applies unchanged.
pub fn opacity_rgba(p4k: &P4k, base: &str, cap: usize) -> Option<(usize, usize, Vec<u8>)> {
    let t = open_dds(p4k, base)?;
    let (_, w, h, px) = level_rgba(&t, cap)?;
    let mut out = vec![0xFFu8; 4 * w * h];
    for i in 0..w * h {
        out[i * 4 + 3] = px[i * 4];
    }
    Some((w, h, out))
}

pub fn opacity_png(p4k: &P4k, base: &str, cap: usize) -> Option<(usize, usize, Vec<u8>)> {
    let (w, h, px) = opacity_rgba(p4k, base, cap)?;
    Some((w, h, png_rgba(w as u32, h as u32, &px)))
}

/// Share of the diffuse map's texels with alpha >= `cut` (the open fraction of a grate / mesh / fence), measured on the largest mip
/// of at most 512k texels (BC1 / BC2 / BC3 / BC7: `dds::bc1_alpha_coverage` ..., the BC7 one approximates mode 7); None when the
/// format is anything else or the mip is not readable.  The viewer keeps this coverage on the far mips instead of the blurred mean
/// alpha (the rule is INFERRED, the engine's own is unknown).
pub fn cutout_coverage(p4k: &P4k, path: &str, cut: f64) -> Option<f64> {
    let tex = open_dds(p4k, &texture_base(path))?;
    let name = tex.format.name;
    if !matches!(name, "bc1" | "bc2" | "bc3" | "bc7") || tex.slices(0) != 1 {
        return None;
    }
    let lv = (0..tex.mips).find(|&i| tex.dims[i].0 * tex.dims[i].1 <= 524288)?;
    let data = tex.level(lv)?;
    let (w, h) = tex.dims[lv];
    match name {
        "bc1" => dds::bc1_alpha_coverage(&data, w, h, cut),
        "bc7" => dds::bc7_alpha_coverage(&data, w, h, cut),
        _ => dds::bc2_bc3_alpha_coverage(name, &data, w, h, cut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real archive: the BC7 crate decal decodes to a PNG-able image and its decoded alpha coverage matches the stand-alone
    /// `bc7_alpha_coverage` helper (which approximates mode 7).  Skips without the archive.
    #[test]
    fn real_archive_bc7_decal_decodes() {
        let path = p4k::default_p4k_path();
        let Ok(p4k) = P4k::open(&path) else { return };
        let Some(tex) = open_dds(&p4k, "Data/Objects/buildingsets/human/lowtech/prop/textures/decal/lowtech_decal_1_a_diff") else { return };
        assert_eq!(tex.format.name, "bc7");
        let (lvl, w, h, px) = level_rgba(&tex, 512).expect("bc7 level decodes");
        assert_eq!(px.len(), w * h * 4);
        let raw = tex.level(lvl).unwrap();
        let t0 = std::time::Instant::now();
        let again = dds::decode("bc7", &raw, w, h).unwrap();
        let dt = t0.elapsed().as_secs_f64();
        eprintln!("decal mip {} {}x{}: {:.1} MB/s of RGBA", lvl, w, h, (w * h * 4) as f64 / 1e6 / dt.max(1e-9));
        assert!(again == px);
        let open = px.chunks_exact(4).filter(|p| p[3] >= 128).count() as f64 / (w * h) as f64;
        let want = dds::bc7_alpha_coverage(&raw, w, h, 0.5).unwrap();
        eprintln!("alpha coverage: decoded {:.4}, helper {:.4}", open, want);
        assert!((open - want).abs() < 0.03, "decoded {} vs helper {}", open, want);
        let png = png_rgba(w as u32, h as u32, &px);
        assert_eq!(p4k_formats::png::decode_png_rgba(&png).unwrap().2, px);
    }

    #[test]
    fn cube_cross_places_faces() {
        let faces: Vec<Vec<u8>> = (0..6u8).map(|f| [f * 10, f, 0, 255].repeat(4)).collect();
        let (w, h, px) = cube_cross(2, 2, &faces);
        assert_eq!((w, h), (8, 6));
        let at = |x: usize, y: usize| px[(y * w + x) * 4];
        assert_eq!(at(2, 0), 20); // +Y above +Z
        assert_eq!(at(0, 2), 10); // -X left
        assert_eq!(at(2, 2), 40); // +Z centre
        assert_eq!(at(4, 2), 0); // +X
        assert_eq!(at(6, 2), 50); // -Z
        assert_eq!(at(2, 4), 30); // -Y below
        assert_eq!(px[(0 * w + 0) * 4 + 3], 255); // empty corner is opaque black
        assert_eq!(&px[0..3], &[0, 0, 0]);
    }

    /// Real archive: the BC6H Monorail cube map (256 x 256, 6 faces, 7 mips) opens with its faces, every mip is available and decodes to
    /// 6 faces of non-black HDR content, and the cross has the right size.  Skips without the archive.
    #[test]
    fn real_archive_bc6h_cubemap() {
        let path = p4k::default_p4k_path();
        let Ok(p4k) = P4k::open(&path) else { return };
        let Some(tex) = open_dds(&p4k, "Data/Materials/VFX/TerraPrime/Blocks_Monorail/Monorail_Cubemap_cm") else { return };
        assert_eq!((tex.format.name, tex.faces, tex.mips, tex.width), ("bc6h", 6, 7, 256));
        for l in 0..tex.mips {
            assert!(tex.available(l), "mip {}", l);
            assert_eq!(tex.level(l).unwrap().len(), tex.sizes[l]);
            assert_eq!(tex.slices(l), 6);
        }
        let mut faces = vec![];
        for k in 0..6 {
            let raw = tex.level_slice(0, k).unwrap();
            assert_eq!(raw.len(), 256 * 256);
            let px = dds::decode("bc6h", &raw, 256, 256).unwrap();
            assert!(px.chunks_exact(4).any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8), "face {} is black", k);
            faces.push(px);
        }
        // the base-file mips (4..6, face-major payload) must agree with the side-file mip 3 above them, face by face (mean colour)
        let mean = |l: usize, k: usize| {
            let (w, h) = tex.dims[l];
            let px = dds::decode("bc6h", &tex.level_slice(l, k).unwrap(), w, h).unwrap();
            let n = (w * h) as f64;
            [0, 1, 2].map(|c| px.chunks_exact(4).map(|p| p[c] as f64).sum::<f64>() / n)
        };
        // (the mips are separately filtered levels, not box-downsampled, so only the brightness order of the faces and a loose bound are checked)
        let (m3, m4): (Vec<f64>, Vec<f64>) = (0..6).map(|k| (mean(3, k)[0], mean(4, k)[0])).unzip();
        eprintln!("face mean red mip3 {:?} mip4 {:?}", m3, m4);
        let order = |m: &[f64]| {
            let mut i: Vec<usize> = (0..6).collect();
            i.sort_by(|&a, &b| m[a].partial_cmp(&m[b]).unwrap());
            i
        };
        assert_eq!(order(&m3), order(&m4));
        assert!(m3.iter().zip(&m4).all(|(a, b)| (a - b).abs() < 60.0));
        let (w, h, cross) = cube_cross(256, 256, &faces);
        assert_eq!((w, h, cross.len()), (1024, 768, 1024 * 768 * 4));
    }
}
