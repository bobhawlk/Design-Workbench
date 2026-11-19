//! What the analysis reads off a part's faces: round faces (holes and pins) with their axial
//! extent, flat faces with their outward normals, and a fingerprint for spotting copies.

use lcw_geom::{principal_frame, Axis, Bbox, M4, V3};
use lcw_step::{Face, Solid, Surface};

/// A cylindrical face.
#[derive(Debug, Clone, Copy)]
pub struct Round {
    pub axis: Axis,
    pub radius: f64,
    /// Extent along the axis (projection of the face's vertices), part frame.
    pub lo: f64,
    pub hi: f64,
    /// The material is outside the cylinder (a hole) rather than inside (a pin).
    pub hole: bool,
    /// How far round the face goes (radians, 2π for a full cylinder).
    pub sweep: f64,
    /// For a face that does not go all the way round (the arc of a slot, a D-shaped hole): a
    /// point on it, halfway round and halfway along, part frame. `None`: a full cylinder.
    pub mid: Option<V3>,
}

impl Round {
    pub fn transformed(&self, m: &M4) -> Round {
        let axis = self.axis.transformed(m);
        // Keep the extent along the new canonical direction.
        let a = m.point(self.axis.origin + self.axis.dir * self.lo);
        let b = m.point(self.axis.origin + self.axis.dir * self.hi);
        let (pa, pb) = ((a - axis.origin).dot(axis.dir), (b - axis.origin).dot(axis.dir));
        Round { axis, radius: self.radius, lo: pa.min(pb), hi: pa.max(pb), hole: self.hole, sweep: self.sweep, mid: self.mid.map(|p| m.point(p)) }
    }
    /// The direction from the axis to `p` is one the face covers (always, for a full cylinder):
    /// a point on the circle of the face's radius can still be off a partial face, on the flat
    /// wall of a slot.
    pub fn covers(&self, p: V3, tol: f64) -> bool {
        let Some(mid) = self.mid else { return true };
        let (a, b) = (p - self.axis.project(p), mid - self.axis.project(mid));
        match (a.unit(), b.unit()) {
            (Some(a), Some(b)) => a.dot(b).clamp(-1.0, 1.0).acos() <= self.sweep / 2.0 + tol / self.radius.max(1e-9),
            _ => false,
        }
    }
    /// The point on the face at `along` (axis coordinate) in the direction of `toward` when the
    /// face covers it, else halfway round the face.
    pub fn point_at(&self, along: f64, toward: V3) -> V3 {
        let on_axis = self.axis.origin + self.axis.dir * along;
        let radial = |p: V3| (p - self.axis.project(p)).unit();
        let dir = if self.covers(toward, 0.0) { radial(toward) } else { None }.or_else(|| self.mid.and_then(radial)).unwrap_or_else(|| self.axis.dir.any_perp());
        on_axis + dir * self.radius
    }
    /// The middle of the face on the cylinder (for picking it).
    pub fn axial_mid(&self) -> f64 {
        (self.lo + self.hi) / 2.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Flat {
    pub origin: V3,
    /// Outward.
    pub normal: V3,
    pub area: f64,
}

/// The summary of one part.
#[derive(Debug, Clone, Default)]
pub struct PartFeatures {
    pub rounds: Vec<Round>,
    pub flats: Vec<Flat>,
    pub vertices: Vec<V3>,
    pub bbox: Option<Bbox>,
    pub faces: usize,
    /// Compared first when looking for copies (exact match needed).
    pub fingerprint: String,
}

impl PartFeatures {
    pub fn of(solids: &[Solid]) -> PartFeatures {
        let mut f = PartFeatures::default();
        let mut kinds = [0usize; 6];
        let mut radii = Vec::new();
        for s in solids {
            for face in &s.faces {
                f.faces += 1;
                match &face.surface {
                    Surface::Plane { .. } => {
                        kinds[0] += 1;
                        if let Some(n) = face.plane_normal() {
                            f.flats.push(Flat { origin: face.vertices.first().copied().unwrap_or_default(), normal: n, area: face_area(face, n) });
                        }
                    }
                    Surface::Cylinder { axis, radius } => {
                        kinds[1] += 1;
                        radii.push((radius * 100.0).round() as i64);
                        if let Some(r) = round_of(face, axis, *radius) {
                            f.rounds.push(r);
                        }
                    }
                    Surface::Cone { .. } => kinds[2] += 1,
                    Surface::Sphere { .. } => kinds[3] += 1,
                    Surface::Torus { .. } => kinds[4] += 1,
                    Surface::Other(_) => kinds[5] += 1,
                }
            }
            f.vertices.extend(s.vertices());
        }
        f.bbox = (!f.vertices.is_empty()).then(|| Bbox::of(f.vertices.iter().copied()));
        radii.sort_unstable();
        let spread = principal_frame(&f.vertices).map(|(_, v, _)| v.map(|x| (x.max(0.0).sqrt() * 100.0).round() as i64)).unwrap_or_default();
        f.fingerprint = format!("{:?}|{}|{:?}|{:?}", kinds, f.vertices.len(), radii, spread);
        f
    }

    /// All round faces share one axis and every flat face is square to it: a spacer, a shaft,
    /// a washer. Such a part turning on its own axis changes nothing.
    pub fn axisymmetric(&self) -> bool {
        let Some(first) = self.rounds.first() else { return false };
        self.rounds.iter().all(|r| r.axis.coincident(&first.axis, 0.05)) && self.flats.iter().all(|f| f.normal.parallel(first.axis.dir))
    }
}

/// A polygon area estimate of a planar face: the outer loop's area less its holes' (round holes
/// from their circles).
pub fn face_area(face: &Face, n: V3) -> f64 {
    let poly = |pts: &[V3]| -> f64 {
        if pts.len() < 3 {
            return 0.0;
        }
        let mut acc = V3::ZERO;
        for i in 0..pts.len() {
            acc = acc + pts[i].cross(pts[(i + 1) % pts.len()]);
        }
        (acc.dot(n) / 2.0).abs()
    };
    let mut area = face.loops.first().map_or(0.0, |l| poly(l));
    if face.loops.first().is_some_and(|l| l.len() <= 2) {
        // A disc: its outline is one circle.
        if let Some((_, _, r)) = face.circles.iter().max_by(|a, b| a.2.total_cmp(&b.2)) {
            area = std::f64::consts::PI * r * r;
        }
    }
    for l in face.loops.iter().skip(1) {
        if l.len() >= 3 {
            area -= poly(l);
        }
    }
    area.max(0.0)
}

fn round_of(face: &Face, axis: &Axis, radius: f64) -> Option<Round> {
    let ax = Axis::new(axis.origin, axis.dir)?;
    let pts: Vec<V3> = face.vertices.iter().copied().chain(face.circles.iter().map(|c| c.0)).collect();
    if pts.is_empty() {
        return None;
    }
    let t: Vec<f64> = pts.iter().map(|p| (*p - ax.origin).dot(ax.dir)).collect();
    let (lo, hi) = t.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(*x), b.max(*x)));
    // The surface normal points away from the axis; a face whose normal points into the void
    // towards the axis bounds a hole.
    let hole = !face.same_sense;
    // Sweep: full when bounded by whole circles (a single-vertex loop), else from the vertices.
    let full = face.loops.iter().any(|l| l.len() <= 1) || face.circles.iter().filter(|c| (c.2 - radius).abs() < 1e-6).count() >= 2 && face.vertices.len() <= 2;
    let mut mid = None;
    let sweep = if full {
        std::f64::consts::TAU
    } else {
        let u = ax.dir.any_perp();
        let w = ax.dir.cross(u);
        let mut angles: Vec<f64> = face
            .vertices
            .iter()
            .filter_map(|p| {
                let r = *p - ax.project(*p);
                (r.norm() > 1e-9).then(|| r.dot(w).atan2(r.dot(u)))
            })
            .collect();
        angles.sort_by(f64::total_cmp);
        // Every vertex at one angle (or none off the axis): the face is bounded by its circles
        // and a seam line, so it goes all the way round.
        let seam = angles.last().zip(angles.first()).is_none_or(|(l, f)| l - f < 1e-9);
        if seam {
            std::f64::consts::TAU
        } else {
            // The largest gap between consecutive angles is the part not covered; the face
            // starts where it ends.
            let (mut gap, mut start): (f64, f64) = (0.0, angles[0]);
            for i in 0..angles.len() {
                let next = if i + 1 < angles.len() { angles[i + 1] } else { angles[0] + std::f64::consts::TAU };
                if next - angles[i] > gap {
                    gap = next - angles[i];
                    start = next;
                }
            }
            let sweep = std::f64::consts::TAU - gap;
            let m = start + sweep / 2.0;
            mid = Some(ax.origin + ax.dir * ((lo + hi) / 2.0) + (u * m.cos() + w * m.sin()) * radius);
            sweep
        }
    };
    Some(Round { axis: ax, radius, lo, hi, hole, sweep, mid })
}

/// A point inside a planar face, away from its edges and holes, for picking it.
pub fn interior_point(face: &Face) -> Option<V3> {
    let n = face.plane_normal()?;
    let outer = face.loops.first()?;
    let origin = *outer.first()?;
    let u = n.any_perp();
    let v = n.cross(u);
    let to2 = |p: V3| ((p - origin).dot(u), (p - origin).dot(v));
    let poly: Vec<(f64, f64)> = outer.iter().map(|p| to2(*p)).collect();
    let holes: Vec<Vec<(f64, f64)>> = face.loops.iter().skip(1).filter(|l| l.len() >= 3).map(|l| l.iter().map(|p| to2(*p)).collect()).collect();
    let circles: Vec<((f64, f64), f64)> = face.circles.iter().map(|c| (to2(c.0), c.2)).collect();
    let inside = |pt: (f64, f64), poly: &[(f64, f64)]| -> bool {
        let mut c = false;
        for i in 0..poly.len() {
            let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
            if (a.1 > pt.1) != (b.1 > pt.1) && pt.0 < (b.0 - a.0) * (pt.1 - a.1) / (b.1 - a.1) + a.0 {
                c = !c;
            }
        }
        c
    };
    if poly.len() < 3 {
        return None;
    }
    let mut edges: Vec<usize> = (0..poly.len()).collect();
    let len = |i: usize| {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()
    };
    edges.sort_by(|a, b| len(*b).total_cmp(&len(*a)));
    for i in edges.into_iter().take(12) {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let l = len(i);
        if l < 1e-6 {
            continue;
        }
        let m = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        let (nx, ny) = (-(b.1 - a.1) / l, (b.0 - a.0) / l);
        for d in [0.6, 1.5, 3.0, 0.25] {
            for s in [1.0, -1.0] {
                let p = (m.0 + nx * d * s, m.1 + ny * d * s);
                let clear_of_round = circles.iter().all(|(c, r)| ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() > r + 0.3);
                if inside(p, &poly) && holes.iter().all(|h| !inside(p, h)) && clear_of_round {
                    return Some(origin + u * p.0 + v * p.1);
                }
            }
        }
    }
    None
}

/// A point on a cylindrical face, in the middle of its arc and its length.
pub fn round_face_point(face: &Face) -> Option<V3> {
    let Surface::Cylinder { axis, radius } = &face.surface else { return None };
    let ax = Axis { origin: axis.origin, dir: axis.dir.unit()? };
    // The middle of the longest straight boundary edge running along the axis: where a bend
    // cylinder meets the flat beside it. It is a line of the tessellation too, so a pick there
    // is exact; a point in the middle of the arc can lie off a coarse mesh by more than a pick's
    // tolerance.
    let mut best: Option<(f64, V3)> = None;
    for lp in &face.loops {
        for i in 0..lp.len() {
            let (a, b) = (lp[i], lp[(i + 1) % lp.len()]);
            let d = b - a;
            let len = d.norm();
            if len < 1e-6 || d.cross(ax.dir).norm() > 1e-4 * len {
                continue;
            }
            if best.is_none_or(|(l, _)| len > l) {
                best = Some((len, (a + b) * 0.5));
            }
        }
    }
    if let Some((_, p)) = best {
        return Some(p);
    }
    let n = face.vertices.len().max(1) as f64;
    let mean = face.vertices.iter().fold(V3::ZERO, |a, p| a + *p) / n;
    let foot = ax.project(mean);
    let radial = (mean - foot).unit()?;
    Some(foot + radial * *radius)
}

/// `p` moved to the pickable point of the bend cylinder it lies on (within `tol` of its surface
/// and its axial extent), or `p` unchanged when it lies on none.
pub fn snap_to_bend(faces: &[Face], p: V3, tol: f64) -> V3 {
    for f in faces {
        let Surface::Cylinder { axis, radius } = &f.surface else { continue };
        let Some(dir) = axis.dir.unit() else { continue };
        let ax = Axis { origin: axis.origin, dir };
        let foot = ax.project(p);
        if ((p - foot).norm() - radius).abs() > tol {
            continue;
        }
        let t: Vec<f64> = f.vertices.iter().map(|q| (*q - ax.origin).dot(dir)).collect();
        let tp = (p - ax.origin).dot(dir);
        let lo = t.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = t.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if tp < lo - tol || tp > hi + tol {
            continue;
        }
        if let Some(q) = round_face_point(f) {
            return q;
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcw_geom::v3;

    fn cylinder(vertices: Vec<V3>, loops: Vec<Vec<V3>>, circles: Vec<(V3, V3, f64)>) -> Face {
        let axis = Axis::new(V3::ZERO, V3::Z).unwrap();
        Face { surface: Surface::Cylinder { axis, radius: 2.0 }, same_sense: false, edges: Vec::new(), vertices, loops, circles }
    }

    #[test]
    fn a_hole_with_a_seam_goes_all_the_way_round() {
        // Two circles joined by a seam line: every vertex at one angle.
        let (a, b) = (v3(2.0, 0.0, 0.0), v3(2.0, 0.0, 3.0));
        let f = cylinder(vec![a, a, b, b], vec![vec![a, a, b, b]], vec![(V3::ZERO, V3::Z, 2.0), (v3(0.0, 0.0, 3.0), V3::Z, 2.0)]);
        let r = round_of(&f, &Axis::new(V3::ZERO, V3::Z).unwrap(), 2.0).unwrap();
        assert!((r.sweep - std::f64::consts::TAU).abs() < 1e-9 && r.mid.is_none(), "{r:?}");
        assert!(r.covers(v3(-2.0, 0.0, 1.0), 0.01));
    }

    #[test]
    fn the_arc_of_a_slot_covers_only_its_own_side() {
        // The arc end of an invented slot: a quarter turn from +x to +y, 3 mm deep.
        let (p, q) = (v3(2.0, 0.0, 0.0), v3(0.0, 2.0, 0.0));
        let (p2, q2) = (v3(2.0, 0.0, 3.0), v3(0.0, 2.0, 3.0));
        let f = cylinder(vec![p, q, q2, p2], vec![vec![p, q, q2, p2]], Vec::new());
        let r = round_of(&f, &Axis::new(V3::ZERO, V3::Z).unwrap(), 2.0).unwrap();
        assert!((r.sweep - std::f64::consts::FRAC_PI_2).abs() < 1e-9, "{r:?}");
        let mid = r.mid.unwrap();
        assert!((mid - v3(2f64.sqrt(), 2f64.sqrt(), 1.5)).norm() < 1e-9, "{mid:?}");
        // A point on the slot's circle but on its flat side is not on the face.
        assert!(r.covers(v3(1.0, 1.0, 0.0), 0.01));
        assert!(!r.covers(v3(0.0, -2.0, 0.0), 0.01));
        // Picked on the arc at the same place along the axis instead.
        let at = r.point_at(0.0, v3(0.0, -2.0, 0.0));
        assert!((at - v3(2f64.sqrt(), 2f64.sqrt(), 0.0)).norm() < 1e-9, "{at:?}");
        // Moved with the part.
        let m = M4::translation(v3(10.0, 0.0, 0.0));
        assert!((r.transformed(&m).mid.unwrap() - (mid + v3(10.0, 0.0, 0.0))).norm() < 1e-9);
    }
}
