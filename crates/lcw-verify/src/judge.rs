//! Comparing one placed part with what the bundle says it should be. Pure functions on the
//! numbers SolidCraft reported and the bundle's matrices.

use lcw_geom::{Bbox, M4, V3};

use crate::obs::{bbox_corners, Geo};

#[derive(Debug, Clone)]
pub struct Tol {
    /// Positions (mm): the larger of this and `pos_rel` of the part's diagonal. SolidCraft's
    /// centre of mass and box come out up to about 5e-5 of the part size off.
    pub pos_mm: f64,
    pub pos_rel: f64,
    /// Orientation of an occurrence's frame against the bundle's (degrees).
    pub angle_deg: f64,
    /// Relative difference of the inertia tensors (per unit mass).
    pub inertia_rel: f64,
    /// A built design's volume against SolidCraft's own import of the same STEP.
    pub volume_rel: f64,
    /// Against the volume Inventor reported.
    pub bundle_volume_rel: f64,
    /// A built design's box against SolidCraft's own import (mm).
    pub bbox_mm: f64,
    /// Against Inventor's range box, which is not tight on curved parts: the larger of this
    /// (mm) and `bundle_bbox_rel` of the box diagonal.
    pub bundle_bbox_mm: f64,
    pub bundle_bbox_rel: f64,
    /// An interference is suspect when it is this fraction of the smaller body.
    pub interference_fail_fraction: f64,
    pub interference_budget_s: f64,
}

impl Default for Tol {
    fn default() -> Tol {
        Tol {
            pos_mm: 0.05,
            pos_rel: 1e-4,
            angle_deg: 0.05,
            inertia_rel: 0.02,
            volume_rel: 1e-3,
            bundle_volume_rel: 5e-3,
            bbox_mm: 0.05,
            bundle_bbox_mm: 1.0,
            bundle_bbox_rel: 0.01,
            interference_fail_fraction: 0.5,
            interference_budget_s: 600.0,
        }
    }
}

/// Everything known about one pairing of a bundle placement with a built instance.
#[derive(Debug, Clone, Default)]
pub struct Judged {
    /// Distance between the two occurrence frames' origins and the angle between their axes.
    pub frame_mm: f64,
    pub frame_deg: f64,
    pub frame_ok: bool,
    /// Distance between the expected and the built centre of mass.
    pub com_mm: Option<f64>,
    pub inertia_rel: Option<f64>,
    pub volume_rel: Option<f64>,
    /// How far the built box lies outside what the bundle's placement allows (mm), 0 when inside.
    pub bbox_excess_mm: Option<f64>,
    /// What decided: "centre of mass", "box", "frame".
    pub method: &'static str,
    pub geometry_ok: bool,
    pub ok: bool,
    /// The geometry is where it should be although the frame is not (a rotation that maps the
    /// part onto itself, a part merged into an identical one that has another frame).
    pub equivalent: bool,
    pub reason: String,
}

pub fn rel_diff(a: f64, b: f64) -> f64 {
    (a - b).abs() / a.abs().max(b.abs()).max(1e-12)
}

fn mat_mul(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    r
}

/// R · I · Rᵀ for the rotation part of `m`.
pub fn rotate_inertia(m: &M4, i: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut r = [[0.0; 3]; 3];
    for (a, row) in r.iter_mut().enumerate() {
        for (b, cell) in row.iter_mut().enumerate() {
            *cell = m.0[a][b];
        }
    }
    let mut rt = [[0.0; 3]; 3];
    for a in 0..3 {
        for b in 0..3 {
            rt[a][b] = r[b][a];
        }
    }
    mat_mul(&mat_mul(&r, i), &rt)
}

fn frob(a: &[[f64; 3]; 3]) -> f64 {
    a.iter().flatten().map(|x| x * x).sum::<f64>().sqrt()
}

/// The rotation of `m` maps the axes onto the axes (a multiple of a quarter turn).
pub fn axis_aligned(m: &M4) -> bool {
    (0..3).all(|c| {
        let v = m.col(c);
        let big = [v.x.abs(), v.y.abs(), v.z.abs()].iter().filter(|x| (**x - 1.0).abs() < 1e-6).count();
        let small = [v.x.abs(), v.y.abs(), v.z.abs()].iter().filter(|x| **x < 1e-6).count();
        big == 1 && small == 2
    })
}

/// The box of a box placed by `m` (the box of its eight corners).
pub fn transformed_box(b: &Bbox, m: &M4) -> Bbox {
    Bbox::of(bbox_corners(b).iter().map(|c| m.point(*c)))
}

/// Frame difference between the bundle's placement and the built one: (mm, degrees).
pub fn frame_delta(expected: &M4, built: &M4) -> (f64, f64) {
    let d = expected.origin().dist(built.origin());
    let rel = expected.orthonormalised().inverse_rigid() * built.orthonormalised();
    let (angle, _) = rel.rotation_angle_axis();
    (d, angle.to_degrees())
}

/// Judge a built instance (`built`, measured in the world) against the part as SolidCraft
/// imported its STEP (`reference`, in the part's own frame) placed by the bundle's `expected`
/// world matrix. `verts` are the STEP's vertices in the part's frame (empty when unreadable),
/// `built_frame` the built instance's own world frame.
pub fn judge(reference: &Geo, verts: &[V3], expected: &M4, built: &Geo, built_frame: &M4, tol: &Tol) -> Judged {
    let (frame_mm, frame_deg) = frame_delta(expected, built_frame);
    let diag = reference.bbox.map_or(0.0, |b| b.diagonal());
    let pos_tol = tol.pos_mm.max(tol.pos_rel * diag);
    let mut j = Judged { frame_mm, frame_deg, frame_ok: frame_mm <= pos_tol && frame_deg <= tol.angle_deg, method: "frame", ..Judged::default() };
    let mut failures: Vec<String> = Vec::new();
    let mut decided = false;

    // Centre of mass.
    if let (Some(rc), Some(bc)) = (reference.com, built.com) {
        let d = expected.point(rc).dist(bc);
        j.com_mm = Some(d);
        j.method = "centre of mass";
        decided = true;
        if d > pos_tol {
            failures.push(format!("centre of mass {d:.3} mm off"));
        }
    }
    // Orientation, through the inertia tensor per unit mass.
    if let (Some(ri), Some(bi), Some(rm), Some(bm)) = (reference.inertia, built.inertia, reference.mass, built.mass) {
        if rm > 0.0 && bm > 0.0 {
            let e = rotate_inertia(expected, &ri);
            let mut diff = [[0.0; 3]; 3];
            let mut en = [[0.0; 3]; 3];
            for a in 0..3 {
                for b in 0..3 {
                    en[a][b] = e[a][b] / rm;
                    diff[a][b] = en[a][b] - bi[a][b] / bm;
                }
            }
            let rel = frob(&diff) / frob(&en).max(1e-12);
            j.inertia_rel = Some(rel);
            decided = true;
            if rel > tol.inertia_rel {
                failures.push(format!("inertia differs by {:.1}% (turned or a different shape)", rel * 100.0));
            }
        }
    }
    // Volume does not depend on the placement.
    if let (Some(rv), Some(bv)) = (reference.volume, built.volume) {
        let r = rel_diff(rv, bv);
        j.volume_rel = Some(r);
        if r > tol.volume_rel.max(1e-3) {
            failures.push(format!("volume differs by {:.2}%", r * 100.0));
        }
    }
    // The box.
    if let (Some(rb), Some(bb)) = (reference.bbox, built.bbox) {
        let upper = transformed_box(&rb, expected);
        let lower = if axis_aligned(expected) {
            upper
        } else if verts.is_empty() {
            Bbox { min: upper.center(), max: upper.center() }
        } else {
            Bbox::of(verts.iter().map(|v| expected.point(*v)))
        };
        let mut excess: f64 = 0.0;
        let (u, l) = (upper, lower);
        for (o_min, o_max, u_min, u_max, l_min, l_max) in [
            (bb.min.x, bb.max.x, u.min.x, u.max.x, l.min.x, l.max.x),
            (bb.min.y, bb.max.y, u.min.y, u.max.y, l.min.y, l.max.y),
            (bb.min.z, bb.max.z, u.min.z, u.max.z, l.min.z, l.max.z),
        ] {
            excess = excess.max(u_min - o_min).max(o_max - u_max).max(o_min - l_min).max(l_max - o_max);
        }
        let excess = (excess - pos_tol).max(0.0);
        j.bbox_excess_mm = Some(excess);
        if !decided {
            j.method = "box";
            decided = true;
        }
        if excess > 0.0 {
            failures.push(format!("box {excess:.3} mm outside where the bundle's placement puts it"));
        }
    }
    let _ = decided;
    j.geometry_ok = failures.is_empty() && (j.com_mm.is_some() || j.bbox_excess_mm.is_some() || j.inertia_rel.is_some());
    if j.method == "frame" {
        // Nothing to measure the geometry by (an open surface body with no box): the frame decides.
        j.geometry_ok = j.frame_ok;
        if !j.frame_ok {
            failures.push(format!("frame {frame_mm:.3} mm and {frame_deg:.3} deg off"));
        }
    }
    j.ok = j.geometry_ok;
    j.equivalent = j.ok && !j.frame_ok && j.method != "frame";
    j.reason = failures.join("; ");
    j
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcw_geom::v3;

    /// An invented 10 x 20 x 30 mm block with its centre at (5, 10, 15), density 1.
    fn block() -> Geo {
        let (a, b, c) = (10.0, 20.0, 30.0);
        let m = a * b * c * 1e-3;
        Geo {
            volume: Some(a * b * c),
            mass: Some(m),
            com: Some(v3(5.0, 10.0, 15.0)),
            inertia: Some([[m / 12.0 * (b * b + c * c), 0.0, 0.0], [0.0, m / 12.0 * (a * a + c * c), 0.0], [0.0, 0.0, m / 12.0 * (a * a + b * b)]]),
            bbox: Some(Bbox { min: v3(0.0, 0.0, 0.0), max: v3(10.0, 20.0, 30.0) }),
            bodies: 1,
            closed: true,
            open_bodies: 0,
        }
    }

    /// What SolidCraft would report for the block after `m`.
    fn placed(g: &Geo, m: &M4) -> Geo {
        Geo {
            com: g.com.map(|c| m.point(c)),
            inertia: g.inertia.map(|i| rotate_inertia(m, &i)),
            bbox: g.bbox.map(|b| transformed_box(&b, m)),
            ..g.clone()
        }
    }

    #[test]
    fn a_part_placed_as_the_bundle_says_passes() {
        let m = M4::rotation(v3(1.0, 2.0, 3.0), V3::Z, 90f64.to_radians());
        let r = block();
        let j = judge(&r, &[], &m, &placed(&r, &m), &m, &Tol::default());
        assert!(j.ok && j.frame_ok && !j.equivalent, "{j:?}");
        assert_eq!(j.method, "centre of mass");
    }

    #[test]
    fn a_shifted_part_is_reported_with_its_distance() {
        let m = M4::translation(v3(100.0, 0.0, 0.0));
        let r = block();
        let moved = M4::translation(v3(103.0, 4.0, 0.0));
        let j = judge(&r, &[], &m, &placed(&r, &moved), &moved, &Tol::default());
        assert!(!j.ok);
        assert!((j.com_mm.unwrap() - 5.0).abs() < 1e-9, "{j:?}");
        assert!(j.reason.contains("centre of mass 5.000 mm off"), "{}", j.reason);
        assert!((j.frame_mm - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_turned_part_is_caught_by_its_inertia_even_when_its_centre_is_right() {
        let r = block();
        // Turned a quarter about its own centre: same centre of mass, different axes.
        let about = M4::rotation(v3(5.0, 10.0, 15.0), V3::Z, 90f64.to_radians());
        let j = judge(&r, &[], &M4::IDENTITY, &placed(&r, &about), &about, &Tol::default());
        assert!(!j.ok, "{j:?}");
        assert!(j.com_mm.unwrap() < 1e-9);
        assert!(j.inertia_rel.unwrap() > 0.1);
        assert!((j.frame_deg - 90.0).abs() < 1e-6);
    }

    #[test]
    fn a_half_turn_that_maps_the_part_onto_itself_is_equivalent_not_wrong() {
        let r = block();
        // Half a turn about the block's own centre line: the same solid, another frame.
        let flip = M4::rotation(v3(5.0, 10.0, 15.0), V3::Z, std::f64::consts::PI);
        let j = judge(&r, &[], &M4::IDENTITY, &placed(&r, &flip), &flip, &Tol::default());
        assert!(j.ok && j.equivalent && !j.frame_ok, "{j:?}");
    }

    #[test]
    fn frames_and_boxes() {
        let a = M4::IDENTITY;
        let b = M4::rotation(V3::ZERO, V3::X, 10f64.to_radians());
        let (mm, deg) = frame_delta(&a, &b);
        assert!(mm < 1e-12 && (deg - 10.0).abs() < 1e-9);
        assert!(axis_aligned(&M4::rotation(V3::ZERO, V3::Z, 90f64.to_radians())));
        assert!(!axis_aligned(&M4::rotation(V3::ZERO, V3::Z, 45f64.to_radians())));
    }

    #[test]
    fn an_open_body_with_no_measurements_is_judged_by_its_frame() {
        let r = Geo { bodies: 1, ..Geo::default() };
        let m = M4::translation(v3(1.0, 2.0, 3.0));
        let ok = judge(&r, &[], &m, &r, &m, &Tol::default());
        assert!(ok.ok && ok.method == "frame");
        let off = judge(&r, &[], &m, &r, &M4::IDENTITY, &Tol::default());
        assert!(!off.ok);
    }
}
