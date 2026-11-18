//! Is a part sheet metal, and how does SolidCraft make it sheet metal again?
//!
//! Inventor's own sheet metal data (thickness, K-factor, bend radius, flat pattern) wins when the
//! bundle has it. Otherwise the part's faces are read: a plate is two large parallel faces a
//! sheet thickness apart; a bend is a pair of coaxial cylinders whose radii differ by that
//! thickness. Either way the result is what `sheet.convert` needs: a point on the fixed face and
//! a point on each bend.

use lcw_bundle::SheetMetal;
use lcw_geom::{Axis, V3};
use lcw_step::{Solid, Surface};

use crate::features::{interior_point, round_face_point};

/// Inventor's default K-factor (used when the source gives none; the report says so).
pub const DEFAULT_K: f64 = 0.44;

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Inventor said so (sheet metal part, its rule).
    Inventor,
    /// Read from the faces.
    Geometry,
}

#[derive(Debug, Clone)]
pub struct SheetPlan {
    pub thickness: f64,
    pub bend_radius: f64,
    pub k_factor: f64,
    pub k_assumed: bool,
    /// A point on the fixed (base) face, part frame.
    pub base_point: V3,
    pub base_normal: V3,
    /// A point on each bend's face, part frame.
    pub bends: Vec<V3>,
    pub source: Source,
    /// The rule's name in SolidCraft (shared by parts with the same values).
    pub rule: String,
    pub material: Option<String>,
    /// Inventor's flat pattern, when it came with the bundle (for the fold rebuild).
    pub flat: Option<lcw_bundle::FlatPattern>,
    pub notes: Vec<String>,
}

pub struct Options {
    /// Thickest plate still treated as sheet metal (mm).
    pub max_thickness: f64,
    /// Treat plain plates (no bends) as sheet metal too.
    pub plates: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options { max_thickness: 6.0, plates: true }
    }
}

/// A rule name from its values: "SM 2 mm R2 K0.44".
pub fn rule_name(material: Option<&str>, t: f64, r: f64, k: f64) -> String {
    let f = |v: f64| {
        let s = format!("{:.3}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    let m = material.map(|m| format!("{} ", m.trim())).unwrap_or_default();
    format!("{m}{} mm R{} K{}", f(t), f(r), f(k))
}

pub fn plan(solids: &[Solid], inventor: Option<&SheetMetal>, material: Option<&str>, opts: &Options) -> Option<SheetPlan> {
    if solids.len() != 1 {
        // One sheet is one body; multi-body parts stay as they are.
        if inventor.is_none() {
            return None;
        }
    }
    let geo = solids.first().and_then(|s| from_geometry(s, inventor.map(|i| i.thickness), opts));
    match inventor {
        Some(sm) => {
            let t = sm.thickness;
            let r = sm.bend_radius.or(geo.as_ref().map(|g| g.bend_radius)).unwrap_or(t);
            let (k, assumed) = match sm.k_factor {
                Some(k) => (k, false),
                None => (DEFAULT_K, true),
            };
            let mut notes = Vec::new();
            if let Some(m) = sm.unfold_method.as_deref().filter(|m| !m.eq_ignore_ascii_case("linear")) {
                notes.push(format!("Inventor unfolds this part with a {m} rule; SolidCraft uses a K-factor. Check the flat size."));
            }
            if assumed {
                notes.push(format!("no K-factor in the export; {DEFAULT_K} assumed"));
            }
            let (base_point, base_normal) = match (sm.base_point, sm.base_normal) {
                (Some(p), Some(n)) => (p, n),
                _ => match &geo {
                    Some(g) => (g.base_point, g.base_normal),
                    None => return None,
                },
            };
            let bends = if !sm.bend_points.is_empty() {
                // Inventor's points are anywhere on a bend face; move each to its tangent edge.
                let faces: &[_] = solids.first().map(|s| s.faces.as_slice()).unwrap_or(&[]);
                sm.bend_points.iter().map(|p| crate::features::snap_to_bend(faces, *p, (t * 0.05).max(0.01))).collect()
            } else { geo.as_ref().map(|g| g.bends.clone()).unwrap_or_default() };
            Some(SheetPlan {
                thickness: t,
                bend_radius: r,
                k_factor: k,
                k_assumed: assumed,
                base_point,
                base_normal,
                bends,
                source: Source::Inventor,
                rule: rule_name(material, t, r, k),
                material: material.map(str::to_string),
                flat: sm.flat.clone(),
                notes,
            })
        }
        None => {
            let g = geo?;
            if g.bends.is_empty() && !opts.plates {
                return None;
            }
            Some(SheetPlan { rule: rule_name(material, g.thickness, g.bend_radius, DEFAULT_K), material: material.map(str::to_string), ..g })
        }
    }
}

/// Read a plate or a bent sheet off a solid's faces.
fn from_geometry(s: &Solid, known_t: Option<f64>, opts: &Options) -> Option<SheetPlan> {
    let mut flats: Vec<(usize, V3, V3, f64)> = Vec::new(); // face, origin, outward normal, area
    for (i, f) in s.faces.iter().enumerate() {
        if let (Surface::Plane { origin, .. }, Some(n)) = (&f.surface, f.plane_normal()) {
            flats.push((i, *origin, n, crate::features::face_area(f, n)));
        }
    }
    flats.sort_by(|a, b| b.3.total_cmp(&a.3));
    let &(base, base_origin, base_n, base_area) = flats.first()?;
    // The sheet thickness: the nearest opposite parallel face behind the largest one.
    let thickness = match known_t {
        Some(t) => t,
        None => flats
            .iter()
            .skip(1)
            .filter(|f| f.2.dot(base_n) < -0.99999)
            .map(|f| (f.1 - base_origin).dot(base_n))
            .filter(|d| *d < -1e-4 && -*d <= opts.max_thickness)
            .map(|d| -d)
            .fold(None, |acc: Option<f64>, d| Some(acc.map_or(d, |a: f64| a.min(d))))?,
    };
    let tol = (thickness * 0.02).max(0.01);
    // Every vertex of a flat plate lies between the two faces.
    let all_v = s.vertices();
    let slab = |n: V3, o: V3| all_v.iter().all(|v| {
        let d = (*v - o).dot(n);
        d <= tol && d >= -thickness - tol
    });
    // Bends: coaxial cylinder pairs whose radii differ by the thickness.
    let cyls: Vec<(usize, Axis, f64)> = s
        .faces
        .iter()
        .enumerate()
        .filter_map(|(i, f)| match &f.surface {
            Surface::Cylinder { axis, radius } => Some((i, Axis::new(axis.origin, axis.dir)?, *radius)),
            _ => None,
        })
        .collect();
    let mut bends = Vec::new();
    let mut radius = None;
    let mut used = vec![false; cyls.len()];
    for a in 0..cyls.len() {
        if used[a] {
            continue;
        }
        for b in 0..cyls.len() {
            if a == b || used[b] {
                continue;
            }
            let (ia, axa, ra) = cyls[a];
            let (_, axb, rb) = cyls[b];
            // A bend runs along the sheet: its faces are much longer along the axis than the
            // sheet is thick (a rounded end or a boss round a hole is only as long as that).
            let along = |i: usize, ax: &Axis| {
                let t: Vec<f64> = s.faces[i].vertices.iter().map(|p| (*p - ax.origin).dot(ax.dir)).collect();
                t.iter().cloned().fold(f64::NEG_INFINITY, f64::max) - t.iter().cloned().fold(f64::INFINITY, f64::min)
            };
            if rb > ra && ((rb - ra) - thickness).abs() < tol && axa.coincident(&axb, tol.max(0.01)) && along(ia, &axa) > thickness * 1.5 {
                // A hole through the plate is a full cylinder only `thickness` long; a bend is a
                // part of one, as long as the bend.
                if let Some(p) = round_face_point(&s.faces[ia]) {
                    bends.push(p);
                    radius.get_or_insert(ra);
                    used[a] = true;
                    used[b] = true;
                }
                break;
            }
        }
    }
    let is_plate = bends.is_empty() && slab(base_n, base_origin);
    if bends.is_empty() && !is_plate {
        return None;
    }
    // Most of the planar area must come in parallel pairs a thickness apart (no bosses or ribs).
    let total: f64 = flats.iter().map(|f| f.3).sum();
    let paired: f64 = flats
        .iter()
        .filter(|f| {
            flats.iter().any(|g| g.2.dot(f.2) < -0.99999 && (((g.1 - f.1).dot(f.2)).abs() - thickness).abs() < tol) || f.3 < base_area * 0.02
        })
        .map(|f| f.3)
        .sum();
    if total > 0.0 && paired / total < 0.8 {
        return None;
    }
    let base_point = interior_point(&s.faces[base])?;
    let bend_count = bends.len();
    Some(SheetPlan {
        thickness,
        bend_radius: radius.unwrap_or(thickness),
        k_factor: DEFAULT_K,
        k_assumed: true,
        base_point,
        base_normal: base_n,
        bends,
        source: Source::Geometry,
        rule: String::new(),
        material: None,
        flat: None,
        notes: vec![format!("recognised from its faces: {} mm sheet{}; K-factor {DEFAULT_K} assumed", fmt(thickness), if is_plate { ", flat plate".to_string() } else { format!(", {bend_count} bend(s)") })],
    })
}

fn fmt(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}
