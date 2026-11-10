//! Tessellation of the engine's analytic physics shapes (box, sphere, cylinder, capsule) into triangles.
//!
//! All functions return (vertices, triangles) with vertices as [x, y, z] and triangles as index triples.  The arithmetic keeps the
//! evaluation order of the original Python implementation so the f64 results match bit for bit where the platform libm agrees.

use std::f64::consts::PI;

use p4k_formats::ivo::{Geom, GeomKind, V3};

pub type Mesh = (Vec<V3>, Vec<[u32; 3]>);

fn norm(v: V3) -> V3 {
    let mut l = (0.0 + v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l == 0.0 {
        l = 1.0;
    }
    [v[0] / l, v[1] / l, v[2] / l]
}

fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// Box: local = Basis*(v-center) (rows are the box axes, verified on real assets), half extents = size.
pub fn tess_box(basis: &[f64; 9], center: V3, size: V3) -> Mesh {
    let ax = [&basis[0..3], &basis[3..6], &basis[6..9]];
    let mut verts = Vec::with_capacity(8);
    for sx in [-1.0, 1.0] {
        for sy in [-1.0, 1.0] {
            for sz in [-1.0, 1.0] {
                let s = [sx * size[0], sy * size[1], sz * size[2]];
                let mut v = [0.0; 3];
                for k in 0..3 {
                    v[k] = center[k] + ax[0][k] * s[0] + ax[1][k] * s[1] + ax[2][k] * s[2];
                }
                verts.push(v);
            }
        }
    }
    // vertex index = sx*4+sy*2+sz (bits); 12 triangles
    let quads = [(0u32, 1, 3, 2), (4, 6, 7, 5), (0, 4, 5, 1), (2, 3, 7, 6), (0, 2, 6, 4), (1, 5, 7, 3)];
    let mut tris = Vec::with_capacity(12);
    for (a, b, c, d) in quads {
        tris.push([a, b, c]);
        tris.push([a, c, d]);
    }
    (verts, tris)
}

fn frame(axis: V3) -> (V3, V3, V3) {
    let a = norm(axis);
    let t = if a[0].abs() < 0.9 { [1.0, 0.0, 0.0] } else { [0.0, 1.0, 0.0] };
    let u = norm(cross(a, t));
    let v = cross(a, u);
    (a, u, v)
}

pub fn tess_cyl(center: V3, axis: V3, hh: f64, r: f64, capsule: bool) -> Mesh {
    let (seg, rings) = (24usize, 6usize);
    let (a, u, v) = frame(axis);
    let mut verts: Vec<V3> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    let mut profile: Vec<(f64, f64)> = Vec::new(); // (h, radius)
    if capsule {
        for i in (0..=rings).rev() {
            // bottom cap, from pole to equator
            let th = (PI / 2.0) * i as f64 / rings as f64;
            profile.push((-hh - r * th.sin(), r * th.cos()));
        }
        for i in 0..=rings {
            let th = (PI / 2.0) * i as f64 / rings as f64;
            profile.push((hh + r * th.sin(), r * th.cos()));
        }
    } else {
        profile = vec![(-hh, 0.0), (-hh, r), (hh, r), (hh, 0.0)];
    }
    let mut rbase: Vec<u32> = Vec::new();
    for &(h, rad) in &profile {
        if rad < 1e-9 {
            verts.push([center[0] + a[0] * h, center[1] + a[1] * h, center[2] + a[2] * h]);
            rbase.push(verts.len() as u32 - 1);
        } else {
            let base = verts.len() as u32;
            for i in 0..seg {
                let ang = 2.0 * PI * i as f64 / seg as f64;
                let (c, s) = (ang.cos(), ang.sin());
                verts.push([
                    center[0] + a[0] * h + (u[0] * c + v[0] * s) * rad,
                    center[1] + a[1] * h + (u[1] * c + v[1] * s) * rad,
                    center[2] + a[2] * h + (u[2] * c + v[2] * s) * rad,
                ]);
            }
            rbase.push(base);
        }
    }
    for j in 0..profile.len() - 1 {
        for i in 0..seg as u32 {
            let i2 = (i + 1) % seg as u32;
            let (p0, p1) = (profile[j], profile[j + 1]);
            if p0.1 < 1e-9 {
                tris.push([rbase[j], rbase[j + 1] + i, rbase[j + 1] + i2]);
            } else if p1.1 < 1e-9 {
                tris.push([rbase[j] + i, rbase[j + 1], rbase[j] + i2]);
            } else {
                tris.push([rbase[j] + i, rbase[j + 1] + i, rbase[j + 1] + i2]);
                tris.push([rbase[j] + i, rbase[j + 1] + i2, rbase[j] + i2]);
            }
        }
    }
    (verts, tris)
}

pub fn tess_sphere(center: V3, r: f64) -> Mesh {
    let (seg, rings) = (20u32, 12u32);
    let mut verts = Vec::new();
    let mut tris = Vec::new();
    for j in 0..=rings {
        let th = PI * j as f64 / rings as f64;
        for i in 0..seg {
            let ph = 2.0 * PI * i as f64 / seg as f64;
            verts.push([center[0] + r * th.sin() * ph.cos(), center[1] + r * th.sin() * ph.sin(), center[2] + r * th.cos()]);
        }
    }
    for j in 0..rings {
        for i in 0..seg {
            let i2 = (i + 1) % seg;
            let (a, b) = (j * seg + i, j * seg + i2);
            let (c, d) = ((j + 1) * seg + i, (j + 1) * seg + i2);
            tris.push([a, c, d]);
            tris.push([a, d, b]);
        }
    }
    (verts, tris)
}

/// Triangles of a geometry (None for kinds that cannot be drawn).
pub fn geom_to_mesh(g: &Geom) -> Option<Mesh> {
    match &g.kind {
        GeomKind::Box { basis, center, size, .. } => Some(tess_box(basis, *center, *size)),
        GeomKind::Sphere { center, radius } => Some(tess_sphere(*center, *radius)),
        GeomKind::Cyl { center, axis, radius, half_height, capsule } => Some(tess_cyl(*center, *axis, *half_height, *radius, *capsule)),
        GeomKind::Mesh { verts, tris } | GeomKind::Cluster { verts, tris } => Some((verts.clone(), tris.clone())),
        GeomKind::Unsupported(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_box_has_eight_corners_and_twelve_triangles() {
        let id = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let (v, t) = tess_box(&id, [1.0, 2.0, 3.0], [0.5, 0.5, 0.5]);
        assert_eq!((v.len(), t.len()), (8, 12));
        assert_eq!(v[0], [0.5, 1.5, 2.5]);
        assert_eq!(v[7], [1.5, 2.5, 3.5]);
    }

    #[test]
    fn sphere_and_cylinder_counts() {
        let (v, t) = tess_sphere([0.0; 3], 2.0);
        assert_eq!((v.len(), t.len()), (13 * 20, 12 * 20 * 2));
        let (v, t) = tess_cyl([0.0; 3], [0.0, 0.0, 1.0], 1.0, 0.5, false);
        assert_eq!((v.len(), t.len()), (2 + 2 * 24, 24 + 2 * 24 + 24));
        let (v, _) = tess_cyl([0.0; 3], [0.0, 0.0, 1.0], 1.0, 0.5, true);
        assert!(v.iter().all(|p| p[2].abs() <= 1.5 + 1e-9));
    }
}
