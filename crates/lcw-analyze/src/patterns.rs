//! Patterns among the occurrences of one part in one assembly: in a row, in a grid, round an
//! axis. A pattern needs at least three members; Inventor's own patterns (bundle) are kept as
//! they are.

use lcw_geom::{Axis, M4, V3};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// `count` copies `spacing` apart along `dir`.
    Linear { dir: V3, spacing: f64, count: usize },
    /// A grid: `count1` along `dir1` and `count2` along `dir2`.
    Rectangular { dir1: V3, spacing1: f64, count1: usize, dir2: V3, spacing2: f64, count2: usize },
    /// `count` copies `step` radians apart about `axis`.
    Circular { axis: Axis, step: f64, count: usize },
}

#[derive(Debug, Clone)]
pub struct Found {
    pub kind: Kind,
    /// Occurrence indices in pattern order, seed first; for a grid, row by row.
    pub members: Vec<usize>,
    /// Each member's index along the pattern ([i] or [i, j]).
    pub index: Vec<Vec<i64>>,
}

/// Look for a pattern among `placements` (occurrence index, placement in the assembly).
pub fn detect(placements: &[(usize, M4)], tol: f64) -> Option<Found> {
    if placements.len() < 3 {
        return None;
    }
    let (s0, t0) = placements[0];
    let inv0 = t0.inverse_rigid();
    // The rigid move taking the seed to each member.
    let moves: Vec<(usize, M4)> = placements.iter().map(|(i, t)| (*i, *t * inv0)).collect();
    let same_rotation = moves.iter().all(|(_, q)| q.distance(&M4::IDENTITY, 0.0) >= 0.0 && rot_close(q, &M4::IDENTITY, 1e-5));
    if same_rotation {
        return translational(&moves, s0, tol);
    }
    circular(&moves, tol)
}

fn rot_close(a: &M4, b: &M4, tol: f64) -> bool {
    (0..3).all(|r| (0..3).all(|c| (a.0[r][c] - b.0[r][c]).abs() < tol))
}

fn translational(moves: &[(usize, M4)], seed: usize, tol: f64) -> Option<Found> {
    let offs: Vec<(usize, V3)> = moves.iter().map(|(i, q)| (*i, q.origin())).collect();
    // The shortest non-zero offset sets the first direction.
    let first = offs.iter().filter(|(_, d)| d.norm() > tol).min_by(|a, b| a.1.norm().total_cmp(&b.1.norm()))?;
    // Point the first direction the canonical way, so the order doesn't depend on the seed.
    let u = if first.1.unit()?.canonical_axis().dot(first.1) < 0.0 { -first.1 } else { first.1 };
    let du = u.unit()?;
    let su = u.norm();
    let along = |d: V3| d.dot(du) / su;
    let perp = |d: V3| d - du * d.dot(du);
    if offs.iter().all(|(_, d)| perp(*d).norm() < tol) {
        // A row: every offset a whole multiple of the spacing.
        let mut idx: Vec<(usize, i64)> = Vec::new();
        for (i, d) in &offs {
            let k = along(*d);
            if (k - k.round()).abs() * su > tol {
                return None;
            }
            idx.push((*i, k.round() as i64));
        }
        let min = idx.iter().map(|x| x.1).min()?;
        let max = idx.iter().map(|x| x.1).max()?;
        let mut ks: Vec<i64> = idx.iter().map(|x| x.1).collect();
        ks.sort_unstable();
        ks.dedup();
        if ks.len() != idx.len() || (max - min + 1) as usize != idx.len() {
            return None;
        }
        idx.sort_by_key(|x| x.1);
        let _ = seed;
        let base = idx[0].1;
        return Some(Found {
            kind: Kind::Linear { dir: du, spacing: su, count: idx.len() },
            members: idx.iter().map(|x| x.0).collect(),
            index: idx.iter().map(|x| vec![x.1 - base]).collect(),
        });
    }
    // A grid: the shortest offset off the first direction sets the second.
    let second = offs.iter().filter(|(_, d)| perp(*d).norm() > tol).min_by(|a, b| perp(a.1).norm().total_cmp(&perp(b.1).norm()))?;
    let v = second.1 - du * second.1.dot(du) * 0.0;
    // Solve d = a·u + b·v for each offset (least squares in the plane of u, v).
    let (uu, uv, vv) = (u.dot(u), u.dot(v), v.dot(v));
    let det = uu * vv - uv * uv;
    if det.abs() < 1e-9 {
        return None;
    }
    let mut idx: Vec<(usize, i64, i64)> = Vec::new();
    for (i, d) in &offs {
        let (du_, dv_) = (d.dot(u), d.dot(v));
        let a = (du_ * vv - dv_ * uv) / det;
        let b = (dv_ * uu - du_ * uv) / det;
        let back = u * a.round() + v * b.round();
        if back.dist(*d) > tol {
            return None;
        }
        idx.push((*i, a.round() as i64, b.round() as i64));
    }
    let (amin, amax) = (idx.iter().map(|x| x.1).min()?, idx.iter().map(|x| x.1).max()?);
    let (bmin, bmax) = (idx.iter().map(|x| x.2).min()?, idx.iter().map(|x| x.2).max()?);
    let (n1, n2) = ((amax - amin + 1) as usize, (bmax - bmin + 1) as usize);
    if n1 * n2 != idx.len() || n1 < 2 || n2 < 2 {
        return None;
    }
    idx.sort_by_key(|x| (x.2, x.1));
    Some(Found {
        kind: Kind::Rectangular { dir1: du, spacing1: su, count1: n1, dir2: v.unit()?, spacing2: v.norm(), count2: n2 },
        members: idx.iter().map(|x| x.0).collect(),
        index: idx.iter().map(|x| vec![x.1 - amin, x.2 - bmin]).collect(),
    })
}

fn circular(moves: &[(usize, M4)], tol: f64) -> Option<Found> {
    // Every move must be a turn about one common axis.
    let mut axis: Option<Axis> = None;
    let mut angles: Vec<(usize, f64)> = Vec::new();
    for (i, q) in moves {
        let (a, dir) = q.rotation_angle_axis();
        if a < 1e-6 {
            if q.origin().norm() > tol {
                return None;
            }
            angles.push((*i, 0.0));
            continue;
        }
        // A point on the axis: solve (I - R) p = t in the plane perpendicular to the axis.
        let t = q.origin();
        let u = dir.any_perp();
        let w = dir.cross(u);
        let rot = |p: V3| q.dir(p);
        let (ru, rw) = (u - rot(u), w - rot(w));
        let (a11, a12, a21, a22) = (ru.dot(u), rw.dot(u), ru.dot(w), rw.dot(w));
        let det = a11 * a22 - a12 * a21;
        if det.abs() < 1e-12 {
            return None;
        }
        let (bu, bw) = (t.dot(u), t.dot(w));
        let x = (bu * a22 - bw * a12) / det;
        let y = (a11 * bw - a21 * bu) / det;
        let p = u * x + w * y;
        // No slide along the axis (that would be a helix).
        if t.dot(dir).abs() > tol {
            return None;
        }
        let this = Axis::new(p, dir)?;
        let signed = if this.dir.dot(dir) > 0.0 { a } else { -a };
        match &axis {
            None => axis = Some(this),
            Some(ax) if ax.coincident(&this, tol) => {}
            Some(_) => return None,
        }
        angles.push((*i, signed));
    }
    let axis = axis?;
    // Angles must be whole multiples of the smallest one.
    let step = angles.iter().map(|a| a.1.abs()).filter(|a| *a > 1e-6).fold(f64::INFINITY, f64::min);
    if !step.is_finite() {
        return None;
    }
    let mut idx: Vec<(usize, i64)> = Vec::new();
    let tau = std::f64::consts::TAU;
    for (i, a) in &angles {
        let a = a.rem_euclid(tau);
        let k = a / step;
        if (k - k.round()).abs() > 1e-4 {
            return None;
        }
        idx.push((*i, k.round() as i64));
    }
    let mut ks: Vec<i64> = idx.iter().map(|x| x.1).collect();
    ks.sort_unstable();
    ks.dedup();
    if ks.len() != idx.len() {
        return None;
    }
    idx.sort_by_key(|x| x.1);
    Some(Found { kind: Kind::Circular { axis, step, count: idx.len() }, members: idx.iter().map(|x| x.0).collect(), index: idx.iter().map(|x| vec![x.1]).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcw_geom::v3;

    #[test]
    fn finds_rows_grids_and_circles() {
        let row: Vec<(usize, M4)> = [2, 0, 1, 3].iter().map(|&i| (i, M4::translation(v3(10.0 + 25.0 * i as f64, 5.0, 0.0)))).collect();
        let f = detect(&row, 1e-6).unwrap();
        assert!(matches!(f.kind, Kind::Linear { count: 4, .. }), "{:?}", f.kind);
        assert_eq!(f.members, vec![0, 1, 2, 3]);

        let mut grid = Vec::new();
        for j in 0..2 {
            for i in 0..3 {
                grid.push((grid.len(), M4::translation(v3(20.0 * i as f64, 30.0 * j as f64, 1.0))));
            }
        }
        assert!(matches!(detect(&grid, 1e-6).unwrap().kind, Kind::Rectangular { count1: 3, count2: 2, .. }));

        let seed = M4::translation(v3(50.0, 0.0, 3.0));
        let ring: Vec<(usize, M4)> = (0..6).map(|k| (k, M4::rotation(v3(5.0, 5.0, 0.0), V3::Z, k as f64 * std::f64::consts::PI / 3.0) * seed)).collect();
        let c = detect(&ring, 1e-6).unwrap();
        match c.kind {
            Kind::Circular { axis, count, step } => {
                assert_eq!(count, 6);
                assert!((step - std::f64::consts::PI / 3.0).abs() < 1e-9);
                assert!(axis.distance_to(v3(5.0, 5.0, 0.0)) < 1e-6);
            }
            k => panic!("{k:?}"),
        }
        // Not a pattern: uneven spacing.
        let uneven: Vec<(usize, M4)> = [0.0, 10.0, 25.0].iter().enumerate().map(|(i, x)| (i, M4::translation(v3(*x, 0.0, 0.0)))).collect();
        assert!(detect(&uneven, 1e-6).is_none());
    }
}
