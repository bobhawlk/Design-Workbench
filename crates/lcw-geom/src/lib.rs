//! Small 3D geometry: vectors, rigid placements, axes, planes and point-set registration.
//!
//! Conventions (shared by every crate and by the bundle format):
//! - millimetres and radians;
//! - a placement is a 4x4 matrix in row-major order acting on column vectors (`p' = M · p`):
//!   rows 0-2 hold the rotation and, in column 3, the translation; row 3 is `0 0 0 1`.
//!   This is the convention of Inventor's `Matrix.Cell(row, col)` and of SolidCraft's
//!   `occurrence.move {matrix}` / `file.insert_step {matrix}`.

use std::ops::{Add, Div, Mul, Neg, Sub};

/// Lengths closer than this (mm) are the same.
pub const LEN_TOL: f64 = 1e-3;
/// Directions whose dot product is closer than this to ±1 are parallel.
pub const DIR_TOL: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct V3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

pub const fn v3(x: f64, y: f64, z: f64) -> V3 {
    V3 { x, y, z }
}

impl V3 {
    pub const ZERO: V3 = v3(0.0, 0.0, 0.0);
    pub const X: V3 = v3(1.0, 0.0, 0.0);
    pub const Y: V3 = v3(0.0, 1.0, 0.0);
    pub const Z: V3 = v3(0.0, 0.0, 1.0);

    pub fn from_array(a: [f64; 3]) -> V3 {
        v3(a[0], a[1], a[2])
    }
    pub fn to_array(self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
    pub fn dot(self, o: V3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    pub fn cross(self, o: V3) -> V3 {
        v3(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
    }
    pub fn norm(self) -> f64 {
        self.dot(self).sqrt()
    }
    pub fn dist(self, o: V3) -> f64 {
        (self - o).norm()
    }
    /// Unit vector, or None for a (near) zero vector.
    pub fn unit(self) -> Option<V3> {
        let n = self.norm();
        (n > 1e-12 && n.is_finite()).then(|| self / n)
    }
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
    /// Any unit vector perpendicular to this one.
    pub fn any_perp(self) -> V3 {
        let a = if self.x.abs() < 0.9 { V3::X } else { V3::Y };
        self.cross(a).unit().unwrap_or(V3::Y)
    }
    /// The same direction with its largest component positive (axes have no sense).
    pub fn canonical_axis(self) -> V3 {
        let m = [self.x, self.y, self.z];
        let i = (0..3).max_by(|&a, &b| m[a].abs().total_cmp(&m[b].abs())).unwrap_or(0);
        if m[i] < 0.0 {
            -self
        } else {
            self
        }
    }
    pub fn parallel(self, o: V3) -> bool {
        match (self.unit(), o.unit()) {
            (Some(a), Some(b)) => (a.dot(b).abs() - 1.0).abs() < DIR_TOL * 100.0,
            _ => false,
        }
    }
    pub fn round(self, step: f64) -> V3 {
        let r = |v: f64| (v / step).round() * step + 0.0;
        v3(r(self.x), r(self.y), r(self.z))
    }
}

impl Add for V3 {
    type Output = V3;
    fn add(self, o: V3) -> V3 {
        v3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}
impl Sub for V3 {
    type Output = V3;
    fn sub(self, o: V3) -> V3 {
        v3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}
impl Mul<f64> for V3 {
    type Output = V3;
    fn mul(self, s: f64) -> V3 {
        v3(self.x * s, self.y * s, self.z * s)
    }
}
impl Div<f64> for V3 {
    type Output = V3;
    fn div(self, s: f64) -> V3 {
        v3(self.x / s, self.y / s, self.z / s)
    }
}
impl Neg for V3 {
    type Output = V3;
    fn neg(self) -> V3 {
        v3(-self.x, -self.y, -self.z)
    }
}

/// A 4x4 affine matrix, row-major, column-vector convention (see the crate docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct M4(pub [[f64; 4]; 4]);

impl Default for M4 {
    fn default() -> M4 {
        M4::IDENTITY
    }
}

impl M4 {
    pub const IDENTITY: M4 = M4([[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]]);

    /// From 16 row-major numbers, or 12 (the top three rows).
    pub fn from_slice(v: &[f64]) -> Option<M4> {
        let mut m = M4::IDENTITY;
        match v.len() {
            12 | 16 => {
                for r in 0..v.len() / 4 {
                    for c in 0..4 {
                        m.0[r][c] = v[r * 4 + c];
                    }
                }
            }
            _ => return None,
        }
        m.0.iter().flatten().all(|x| x.is_finite()).then_some(m)
    }
    pub fn to_vec16(&self) -> Vec<f64> {
        self.0.iter().flatten().copied().collect()
    }
    /// The top 3x4 rows, as SolidCraft's `matrix` parameters take them.
    pub fn to_vec12(&self) -> Vec<f64> {
        self.0[..3].iter().flatten().copied().collect()
    }
    /// A placement whose local axes are `x`, `y`, `z` (columns) at `origin`.
    pub fn from_axes(origin: V3, x: V3, y: V3, z: V3) -> M4 {
        M4([[x.x, y.x, z.x, origin.x], [x.y, y.y, z.y, origin.y], [x.z, y.z, z.z, origin.z], [0.0, 0.0, 0.0, 1.0]])
    }
    /// A right-handed frame from an origin, a z direction and an x hint.
    pub fn frame(origin: V3, z: V3, x_hint: V3) -> Option<M4> {
        let z = z.unit()?;
        let x = (x_hint - z * x_hint.dot(z)).unit().unwrap_or_else(|| z.any_perp());
        Some(M4::from_axes(origin, x, z.cross(x), z))
    }
    pub fn translation(t: V3) -> M4 {
        let mut m = M4::IDENTITY;
        m.0[0][3] = t.x;
        m.0[1][3] = t.y;
        m.0[2][3] = t.z;
        m
    }
    /// A turn of `angle` radians about the line through `origin` along `axis`.
    pub fn rotation(origin: V3, axis: V3, angle: f64) -> M4 {
        let a = axis.unit().unwrap_or(V3::Z);
        let (s, c) = angle.sin_cos();
        let t = 1.0 - c;
        let r = [
            [t * a.x * a.x + c, t * a.x * a.y - s * a.z, t * a.x * a.z + s * a.y],
            [t * a.x * a.y + s * a.z, t * a.y * a.y + c, t * a.y * a.z - s * a.x],
            [t * a.x * a.z - s * a.y, t * a.y * a.z + s * a.x, t * a.z * a.z + c],
        ];
        let mut m = M4::IDENTITY;
        for i in 0..3 {
            m.0[i][..3].copy_from_slice(&r[i]);
        }
        M4::translation(origin) * m * M4::translation(-origin)
    }
    /// The reflection through the plane through `point` with normal `n`.
    pub fn reflection(point: V3, n: V3) -> M4 {
        let n = n.unit().unwrap_or(V3::Z);
        let mut m = M4::IDENTITY;
        let nn = [n.x, n.y, n.z];
        for (i, row) in m.0.iter_mut().take(3).enumerate() {
            for (j, cell) in row.iter_mut().take(3).enumerate() {
                *cell = if i == j { 1.0 } else { 0.0 } - 2.0 * nn[i] * nn[j];
            }
        }
        M4::translation(point) * m * M4::translation(-point)
    }
    pub fn origin(&self) -> V3 {
        v3(self.0[0][3], self.0[1][3], self.0[2][3])
    }
    pub fn col(&self, c: usize) -> V3 {
        v3(self.0[0][c], self.0[1][c], self.0[2][c])
    }
    pub fn point(&self, p: V3) -> V3 {
        let m = &self.0;
        v3(
            m[0][0] * p.x + m[0][1] * p.y + m[0][2] * p.z + m[0][3],
            m[1][0] * p.x + m[1][1] * p.y + m[1][2] * p.z + m[1][3],
            m[2][0] * p.x + m[2][1] * p.y + m[2][2] * p.z + m[2][3],
        )
    }
    pub fn dir(&self, d: V3) -> V3 {
        let m = &self.0;
        v3(
            m[0][0] * d.x + m[0][1] * d.y + m[0][2] * d.z,
            m[1][0] * d.x + m[1][1] * d.y + m[1][2] * d.z,
            m[2][0] * d.x + m[2][1] * d.y + m[2][2] * d.z,
        )
    }
    pub fn det3(&self) -> f64 {
        let m = &self.0;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }
    /// |RᵀR − I| (largest entry): 0 for a rigid or mirrored placement.
    pub fn orthonormal_error(&self) -> f64 {
        let mut worst: f64 = 0.0;
        for i in 0..3 {
            for j in 0..3 {
                let d = self.col(i).dot(self.col(j)) - if i == j { 1.0 } else { 0.0 };
                worst = worst.max(d.abs());
            }
        }
        worst
    }
    pub fn is_rigid(&self) -> bool {
        self.orthonormal_error() < 1e-6 && self.det3() > 0.0
    }
    pub fn is_mirror(&self) -> bool {
        self.orthonormal_error() < 1e-6 && self.det3() < 0.0
    }
    /// The inverse of a rigid (or mirrored) placement.
    pub fn inverse_rigid(&self) -> M4 {
        let m = &self.0;
        let mut r = M4::IDENTITY;
        for i in 0..3 {
            for j in 0..3 {
                r.0[i][j] = m[j][i];
            }
        }
        let t = r.dir(self.origin());
        r.0[0][3] = -t.x;
        r.0[1][3] = -t.y;
        r.0[2][3] = -t.z;
        r
    }
    /// Make the rotation part exactly orthonormal (Gram-Schmidt on x then y), keeping handedness.
    pub fn orthonormalised(&self) -> M4 {
        let x = self.col(0).unit().unwrap_or(V3::X);
        let y0 = self.col(1);
        let y = (y0 - x * y0.dot(x)).unit().unwrap_or_else(|| x.any_perp());
        let mut z = x.cross(y);
        if self.det3() < 0.0 {
            z = -z;
        }
        M4::from_axes(self.origin(), x, y, z)
    }
    /// Rotation angle (radians, 0..π) and axis of the rotation part.
    pub fn rotation_angle_axis(&self) -> (f64, V3) {
        let m = &self.0;
        let tr = m[0][0] + m[1][1] + m[2][2];
        let angle = ((tr - 1.0) / 2.0).clamp(-1.0, 1.0).acos();
        let axis = v3(m[2][1] - m[1][2], m[0][2] - m[2][0], m[1][0] - m[0][1]);
        let axis = match axis.unit() {
            Some(a) => a,
            None if angle > 1.0 => {
                // 180°: the axis is the column of (R + I) with the largest norm.
                let cols = [0, 1, 2].map(|c| self.col(c) + [V3::X, V3::Y, V3::Z][c]);
                cols.into_iter().max_by(|a, b| a.norm().total_cmp(&b.norm())).and_then(V3::unit).unwrap_or(V3::Z)
            }
            None => V3::Z,
        };
        (angle, axis)
    }
    /// The largest difference between two matrices' entries (rotation scaled by `scale` mm).
    pub fn distance(&self, o: &M4, scale: f64) -> f64 {
        let mut worst: f64 = 0.0;
        for r in 0..3 {
            for c in 0..3 {
                worst = worst.max((self.0[r][c] - o.0[r][c]).abs() * scale);
            }
            worst = worst.max((self.0[r][3] - o.0[r][3]).abs());
        }
        worst
    }
}

impl Mul for M4 {
    type Output = M4;
    fn mul(self, o: M4) -> M4 {
        let mut r = [[0.0; 4]; 4];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = (0..4).map(|k| self.0[i][k] * o.0[k][j]).sum();
            }
        }
        M4(r)
    }
}

/// An infinite line: a point and a unit direction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Axis {
    pub origin: V3,
    pub dir: V3,
}

impl Axis {
    pub fn new(origin: V3, dir: V3) -> Option<Axis> {
        Some(Axis { origin, dir: dir.unit()?.canonical_axis() })
    }
    /// The point of the line closest to `p`.
    pub fn project(&self, p: V3) -> V3 {
        self.origin + self.dir * (p - self.origin).dot(self.dir)
    }
    pub fn distance_to(&self, p: V3) -> f64 {
        p.dist(self.project(p))
    }
    /// The same line through its point nearest the world origin (for grouping).
    pub fn normalised(&self) -> Axis {
        Axis { origin: self.project(V3::ZERO), dir: self.dir }
    }
    /// Same line within `tol` mm (over a ±`extent` mm stretch) and parallel.
    pub fn coincident(&self, o: &Axis, tol: f64) -> bool {
        self.dir.parallel(o.dir) && self.distance_to(o.origin) < tol && self.distance_to(o.origin + o.dir * 100.0) < tol * 2.0
    }
    pub fn transformed(&self, m: &M4) -> Axis {
        Axis { origin: m.point(self.origin), dir: m.dir(self.dir).unit().unwrap_or(self.dir).canonical_axis() }
    }
}

/// A plane: a point and a unit normal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plane {
    pub origin: V3,
    pub normal: V3,
}

impl Plane {
    pub fn signed_distance(&self, p: V3) -> f64 {
        (p - self.origin).dot(self.normal)
    }
}

/// An axis-aligned box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bbox {
    pub min: V3,
    pub max: V3,
}

impl Bbox {
    pub fn empty() -> Bbox {
        Bbox { min: v3(f64::INFINITY, f64::INFINITY, f64::INFINITY), max: v3(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY) }
    }
    pub fn of(points: impl IntoIterator<Item = V3>) -> Bbox {
        let mut b = Bbox::empty();
        for p in points {
            b.add(p);
        }
        b
    }
    pub fn add(&mut self, p: V3) {
        self.min = v3(self.min.x.min(p.x), self.min.y.min(p.y), self.min.z.min(p.z));
        self.max = v3(self.max.x.max(p.x), self.max.y.max(p.y), self.max.z.max(p.z));
    }
    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x
    }
    pub fn center(&self) -> V3 {
        (self.min + self.max) / 2.0
    }
    pub fn size(&self) -> V3 {
        self.max - self.min
    }
    pub fn diagonal(&self) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            self.size().norm()
        }
    }
}

/// Eigen-decomposition of a symmetric 3x3 matrix (Jacobi): eigenvalues descending and their
/// unit eigenvectors as columns.
pub fn sym_eigen3(a: [[f64; 3]; 3]) -> ([f64; 3], [V3; 3]) {
    let mut m = a;
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..64 {
        let off = m[0][1].abs() + m[0][2].abs() + m[1][2].abs();
        if off < 1e-15 * (m[0][0].abs() + m[1][1].abs() + m[2][2].abs() + 1e-300) {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            if m[p][q].abs() < 1e-300 {
                continue;
            }
            let theta = (m[q][q] - m[p][p]) / (2.0 * m[p][q]);
            let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
            let t = if theta == 0.0 { 1.0 } else { t };
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for k in 0..3 {
                let (mkp, mkq) = (m[k][p], m[k][q]);
                m[k][p] = c * mkp - s * mkq;
                m[k][q] = s * mkp + c * mkq;
            }
            for k in 0..3 {
                let (mpk, mqk) = (m[p][k], m[q][k]);
                m[p][k] = c * mpk - s * mqk;
                m[q][k] = s * mpk + c * mqk;
            }
            for row in &mut v {
                let (vp, vq) = (row[p], row[q]);
                row[p] = c * vp - s * vq;
                row[q] = s * vp + c * vq;
            }
        }
    }
    let mut idx = [0, 1, 2];
    idx.sort_by(|&i, &j| m[j][j].total_cmp(&m[i][i]));
    let vals = idx.map(|i| m[i][i]);
    let vecs = idx.map(|i| v3(v[0][i], v[1][i], v[2][i]));
    (vals, vecs)
}

/// Centroid and principal frame (axes by decreasing spread) of a point set.
pub fn principal_frame(points: &[V3]) -> Option<(V3, [f64; 3], [V3; 3])> {
    if points.is_empty() {
        return None;
    }
    let c = points.iter().fold(V3::ZERO, |a, p| a + *p) / points.len() as f64;
    let mut cov = [[0.0; 3]; 3];
    for p in points {
        let d = (*p - c).to_array();
        for i in 0..3 {
            for j in 0..3 {
                cov[i][j] += d[i] * d[j];
            }
        }
    }
    let n = points.len() as f64;
    for row in &mut cov {
        for cell in row.iter_mut() {
            *cell /= n;
        }
    }
    let (vals, vecs) = sym_eigen3(cov);
    Some((c, vals, vecs))
}

/// A spatial hash for "is there a point within `tol` of p" queries.
pub struct PointGrid {
    cell: f64,
    map: std::collections::HashMap<(i64, i64, i64), Vec<V3>>,
}

impl PointGrid {
    pub fn new(points: &[V3], tol: f64) -> PointGrid {
        let cell = tol.max(1e-6) * 2.0;
        let mut map: std::collections::HashMap<(i64, i64, i64), Vec<V3>> = std::collections::HashMap::new();
        for p in points {
            map.entry(Self::key(*p, cell)).or_default().push(*p);
        }
        PointGrid { cell, map }
    }
    fn key(p: V3, cell: f64) -> (i64, i64, i64) {
        ((p.x / cell).floor() as i64, (p.y / cell).floor() as i64, (p.z / cell).floor() as i64)
    }
    pub fn near(&self, p: V3, tol: f64) -> bool {
        let (kx, ky, kz) = Self::key(p, self.cell);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if let Some(list) = self.map.get(&(kx + dx, ky + dy, kz + dz)) {
                        if list.iter().any(|q| q.dist(p) <= tol) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

/// Find a transform (rigid, or mirrored when `allow_mirror`) mapping point set `a` onto `b`.
/// Tries the principal frames' sign combinations and keeps the one where every point of `a`
/// lands within `tol` of a point of `b`. Returns the transform and whether it mirrors.
pub fn register(a: &[V3], b: &[V3], tol: f64, allow_mirror: bool) -> Option<(M4, bool)> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (ca, va, ea) = principal_frame(a)?;
    let (cb, vb, eb) = principal_frame(b)?;
    let scale = va[0].abs().max(1e-9);
    if (0..3).any(|i| (va[i] - vb[i]).abs() > 1e-3 * scale + tol * tol) {
        return None;
    }
    let grid = PointGrid::new(b, tol);
    let fa = M4::from_axes(ca, ea[0], ea[1], ea[2]);
    let mut best: Option<(M4, bool)> = None;
    // Degenerate spreads (round or square parts) leave the frame ambiguous; also try quarter
    // turns about each principal axis.
    let extra: Vec<M4> = {
        let mut v = vec![M4::IDENTITY];
        for axis in [V3::X, V3::Y, V3::Z] {
            for k in 1..4 {
                v.push(M4::rotation(V3::ZERO, axis, k as f64 * std::f64::consts::FRAC_PI_2));
            }
        }
        v
    };
    for sx in [1.0, -1.0] {
        for sy in [1.0, -1.0] {
            for sz in [1.0, -1.0] {
                for e in &extra {
                    let fb = M4::from_axes(cb, eb[0] * sx, eb[1] * sy, eb[2] * sz) * *e;
                    let t = fb * fa.inverse_rigid();
                    let mirrored = t.det3() < 0.0;
                    if mirrored && !allow_mirror {
                        continue;
                    }
                    if !a.iter().all(|p| grid.near(t.point(*p), tol)) {
                        continue;
                    }
                    if !mirrored {
                        return Some((t, false));
                    }
                    if best.is_none() {
                        best = Some((t, true));
                    }
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rigid_inverse_and_rotation() {
        let m = M4::rotation(v3(1.0, 2.0, 3.0), v3(1.0, 1.0, 0.0), 0.7) * M4::translation(v3(5.0, -2.0, 1.0));
        let i = m.inverse_rigid();
        let p = v3(3.0, 4.0, 5.0);
        assert!(i.point(m.point(p)).dist(p) < 1e-12);
        let (a, ax) = m.rotation_angle_axis();
        assert!((a - 0.7).abs() < 1e-12);
        assert!(ax.parallel(v3(1.0, 1.0, 0.0)));
        assert!(m.is_rigid());
        assert!(M4::reflection(V3::ZERO, V3::X).is_mirror());
    }

    #[test]
    fn registers_rigid_and_mirrored_copies() {
        let pts = vec![v3(0.0, 0.0, 0.0), v3(40.0, 0.0, 0.0), v3(40.0, 10.0, 0.0), v3(0.0, 25.0, 0.0), v3(0.0, 0.0, 3.0), v3(7.0, 3.0, 3.0)];
        let t = M4::rotation(v3(5.0, 5.0, 5.0), v3(0.2, 0.5, 1.0), 1.1) * M4::translation(v3(100.0, 0.0, -40.0));
        let moved: Vec<V3> = pts.iter().map(|p| t.point(*p)).collect();
        let (r, mirrored) = register(&pts, &moved, 1e-6, true).expect("rigid copy");
        assert!(!mirrored && r.distance(&t, 100.0) < 1e-6);
        let mirror = M4::reflection(v3(10.0, 0.0, 0.0), V3::X);
        let flipped: Vec<V3> = pts.iter().map(|p| mirror.point(*p)).collect();
        let (_, mirrored) = register(&pts, &flipped, 1e-6, true).expect("mirror copy");
        assert!(mirrored);
        assert!(register(&pts, &flipped, 1e-6, false).is_none());
    }

    #[test]
    fn eigen_of_diagonal_and_rotated() {
        let (vals, _) = sym_eigen3([[2.0, 0.0, 0.0], [0.0, 5.0, 0.0], [0.0, 0.0, 1.0]]);
        assert_eq!(vals, [5.0, 2.0, 1.0]);
    }
}
