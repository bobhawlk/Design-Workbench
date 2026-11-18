//! Mirror symmetry of an assembly: planes that map pairs of placed parts onto each other (the
//! same part, or a part and its mirror copy).

use lcw_geom::{PointGrid, M4, V3};

use crate::features::PartFeatures;

#[derive(Debug, Clone)]
pub struct Plane {
    pub point: V3,
    pub normal: V3,
    /// Pairs of instance indices the plane maps onto each other.
    pub pairs: Vec<(usize, usize)>,
}

/// `instances[i] = (part feature index, world placement)`; `same(a, b)` says whether two part
/// definitions are the same part or mirror images of each other.
pub fn planes(instances: &[(usize, M4)], features: &[Option<PartFeatures>], same: impl Fn(usize, usize) -> bool, min_pairs: usize) -> Vec<Plane> {
    let mut found: Vec<Plane> = Vec::new();
    let world_pts = |i: usize| -> Vec<V3> {
        let (d, m) = instances[i];
        features[d].as_ref().map(|f| f.vertices.iter().map(|p| m.point(*p)).collect()).unwrap_or_default()
    };
    let centroid = |pts: &[V3]| pts.iter().fold(V3::ZERO, |a, p| a + *p) / pts.len().max(1) as f64;
    let n = instances.len().min(600);
    let pts: Vec<Vec<V3>> = (0..n).map(world_pts).collect();
    let cents: Vec<V3> = pts.iter().map(|p| centroid(p)).collect();
    let mut paired = vec![false; n];
    for a in 0..n {
        if pts[a].len() < 4 || paired[a] {
            continue;
        }
        for b in a + 1..n {
            if paired[b] || pts[b].len() != pts[a].len() || !same(instances[a].0, instances[b].0) {
                continue;
            }
            let d = cents[b] - cents[a];
            let Some(normal) = d.unit() else { continue };
            let normal = normal.canonical_axis();
            let mid = (cents[a] + cents[b]) / 2.0;
            let s = M4::reflection(mid, normal);
            let size = lcw_geom::Bbox::of(pts[a].iter().copied()).diagonal().max(1.0);
            let tol = (size * 1e-4).max(0.01);
            let grid = PointGrid::new(&pts[b], tol);
            if !pts[a].iter().all(|p| grid.near(s.point(*p), tol)) {
                continue;
            }
            paired[a] = true;
            paired[b] = true;
            let offset = mid.dot(normal);
            match found.iter_mut().find(|p| p.normal.dot(normal) > 1.0 - 1e-6 && (p.point.dot(p.normal) - offset).abs() < tol.max(0.05)) {
                Some(p) => p.pairs.push((a, b)),
                None => found.push(Plane { point: mid, normal, pairs: vec![(a, b)] }),
            }
            break;
        }
    }
    found.retain(|p| p.pairs.len() >= min_pairs);
    found.sort_by(|a, b| b.pairs.len().cmp(&a.pairs.len()));
    found
}
