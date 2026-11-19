//! What SolidCraft reports: the component tree with its placements, a design's measurements,
//! the links, the world bodies. Parsing only, with the key names taken from the profile.

use std::collections::BTreeMap;

use lcw_geom::{v3, Bbox, M4, V3};
use lcw_json::{obj, Json};
use lcw_solidcraft::profile::Profile;

use crate::session::Session;

/// Mass properties of what one occurrence (or one saved design) holds, in the frame the answer
/// is in (the world for an occurrence of an assembly).
#[derive(Debug, Clone, Default)]
pub struct Geo {
    pub volume: Option<f64>,
    pub mass: Option<f64>,
    pub com: Option<V3>,
    /// About the centre of mass, in that frame's axes.
    pub inertia: Option<[[f64; 3]; 3]>,
    pub bbox: Option<Bbox>,
    pub bodies: usize,
    /// Every body is a closed solid.
    pub closed: bool,
    /// Open bodies (surfaces) that have no volume.
    pub open_bodies: usize,
}

fn v3_of(j: Option<&Json>) -> Option<V3> {
    lcw_json::to_array::<3>(j?).map(V3::from_array)
}

fn bbox_of(j: Option<&Json>) -> Option<Bbox> {
    let j = j?;
    let (min, max) = (v3_of(j.get("min"))?, v3_of(j.get("max"))?);
    Some(Bbox { min, max })
}

fn m3_of(j: Option<&Json>) -> Option<[[f64; 3]; 3]> {
    let rows = j?.as_array()?;
    if rows.len() != 3 {
        return None;
    }
    let mut m = [[0.0; 3]; 3];
    for (r, row) in rows.iter().enumerate() {
        m[r] = lcw_json::to_array::<3>(row)?;
    }
    Some(m)
}

impl Geo {
    /// From an occurrence-properties answer.
    pub fn from_props(p: &Profile, j: &Json) -> Geo {
        let k = |key: &str| p.vname("props", key);
        let bodies = j.arr_of(&k("bodies"));
        let total = j.get(&k("total")).cloned().unwrap_or_default();
        let closed_n = bodies.iter().filter(|b| b.bool_of(&k("closed")).unwrap_or(false)).count();
        Geo {
            volume: total.f64_of(&k("volume")),
            mass: total.f64_of(&k("mass")),
            com: v3_of(total.get(&k("com"))),
            inertia: m3_of(total.get(&k("inertia"))),
            bbox: bbox_of(total.get(&k("bbox"))),
            bodies: bodies.len(),
            closed: !bodies.is_empty() && closed_n == bodies.len(),
            open_bodies: bodies.len() - closed_n,
        }
    }
}

/// One component of SolidCraft's tree.
#[derive(Debug, Clone, Default)]
pub struct Comp {
    pub id: u64,
    pub name: String,
    /// Occurrence ids placed inside it.
    pub children: Vec<u64>,
}

/// One placement of a component in its parent.
#[derive(Debug, Clone)]
pub struct Occ {
    pub id: u64,
    pub name: String,
    /// The component it places.
    pub comp: u64,
    /// Placement in the parent, column-vector form (SolidCraft's rows are row vectors with the
    /// translation in the last row; this is the transpose).
    pub local: M4,
}

#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub comps: BTreeMap<u64, Comp>,
    pub occs: BTreeMap<u64, Occ>,
}

/// One placed instance, reached through a path of occurrences from the design.
#[derive(Debug, Clone)]
pub struct Inst {
    pub path: Vec<u64>,
    /// The occurrence names along the path.
    pub names: Vec<String>,
    pub comp: u64,
    pub comp_name: String,
    pub world: M4,
    /// Holds no placements of its own (a part).
    pub leaf: bool,
}

impl Inst {
    pub fn name(&self) -> &str {
        self.names.last().map(String::as_str).unwrap_or("")
    }
}

/// A row-vector matrix as SolidCraft writes it (translation in the last row) to a
/// column-vector one.
pub fn from_row_vector(j: &Json) -> Option<M4> {
    let rows = j.as_array()?;
    if rows.len() != 4 {
        return None;
    }
    let mut m = M4::IDENTITY;
    for (r, row) in rows.iter().enumerate() {
        let a = lcw_json::to_array::<4>(row)?;
        for (c, x) in a.iter().enumerate() {
            m.0[c][r] = *x;
        }
    }
    Some(m)
}

impl Tree {
    pub fn parse(p: &Profile, j: &Json) -> Tree {
        let k = |key: &str| p.vname("components", key);
        let mut t = Tree::default();
        for c in j.arr_of(&k("list")) {
            let Some(id) = c.get(&k("id")).and_then(Json::as_u64) else { continue };
            let children = c.arr_of(&k("children")).iter().filter_map(Json::as_u64).collect();
            t.comps.insert(id, Comp { id, name: c.str_of(&k("name")).unwrap_or_default().to_string(), children });
            for o in c.arr_of(&k("occurrences")) {
                let Some(oid) = o.get(&k("occurrence_id")).and_then(Json::as_u64) else { continue };
                let local = o.get(&k("transform")).and_then(from_row_vector).unwrap_or(M4::IDENTITY);
                t.occs.insert(oid, Occ { id: oid, name: o.str_of(&k("occurrence_name")).unwrap_or_default().to_string(), comp: id, local });
            }
        }
        t
    }

    /// The design's own component: the one no occurrence places (id 0 in SolidCraft).
    pub fn root(&self) -> Option<&Comp> {
        let placed: std::collections::BTreeSet<u64> = self.occs.values().map(|o| o.comp).collect();
        self.comps.values().find(|c| !placed.contains(&c.id))
    }

    /// Every instance below the design, with its world placement: the product of the
    /// placements along its path. A component placed twice is expanded under each placement.
    pub fn instances(&self) -> Vec<Inst> {
        let mut out = Vec::new();
        if let Some(root) = self.root() {
            self.expand(root, &M4::IDENTITY, &[], &[], &mut out, 0);
        }
        out
    }

    fn expand(&self, parent: &Comp, world: &M4, path: &[u64], names: &[String], out: &mut Vec<Inst>, depth: usize) {
        if depth > 32 {
            return;
        }
        for oid in &parent.children {
            let Some(o) = self.occs.get(oid) else { continue };
            let Some(c) = self.comps.get(&o.comp) else { continue };
            let w = *world * o.local;
            let (mut p, mut n) = (path.to_vec(), names.to_vec());
            p.push(*oid);
            n.push(o.name.clone());
            out.push(Inst { path: p.clone(), names: n.clone(), comp: c.id, comp_name: c.name.clone(), world: w, leaf: c.children.is_empty() });
            self.expand(c, &w, &p, &n, out, depth + 1);
        }
    }
}

/// The part of `name` before the occurrence counter (`Bracket:2` → `Bracket`).
pub fn name_root(name: &str) -> &str {
    match name.rsplit_once(':') {
        Some((root, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => root,
        _ => name,
    }
}

/// A built name without the " (N)" a design library adds to tell apart two different designs that
/// share a name (two part files both called `lever` become `lever` and `lever (2)`), and without
/// the occurrence number. Which of the same-named parts it is, is then the geometry's to say.
pub fn design_root(name: &str) -> &str {
    let r = name_root(name);
    match r.strip_suffix(')').and_then(|x| x.rsplit_once(" (")) {
        Some((base, n)) if !base.is_empty() && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => base,
        _ => r,
    }
}

/// A component's links: component id → the design file it links.
pub fn parse_links(p: &Profile, j: &Json) -> BTreeMap<u64, String> {
    let k = |key: &str| p.vname("links", key);
    let mut m = BTreeMap::new();
    for l in j.arr_of(&k("list")) {
        if let (Some(c), Some(f)) = (l.get(&k("component")).and_then(Json::as_u64), l.str_of(&k("file"))) {
            m.insert(c, f.to_string());
        }
    }
    m
}

/// What a freshly opened design holds, measured in its own frame.
#[derive(Debug, Clone, Default)]
pub struct Measured {
    pub design_name: String,
    pub geo: Option<Geo>,
    /// Why it could not be opened or measured.
    pub error: Option<String>,
    /// More than one placement at the top (a STEP assembly): only the first was measured.
    pub placements: usize,
}

/// Open a design and measure the one part in it.
pub fn measure_design(s: &mut Session, path: &str) -> Measured {
    let mut m = Measured::default();
    let opened = match s.open(path) {
        Ok(o) => o,
        Err(e) => {
            m.error = Some(e);
            return m;
        }
    };
    m.design_name = opened.str_of(&s.p.vname("open_result", "name")).unwrap_or_default().to_string();
    let list = match s.op("list_components", obj! {"compact" => true, "bodies" => false}) {
        Ok(l) => l,
        Err(e) => {
            m.error = Some(e);
            return m;
        }
    };
    let tree = Tree::parse(s.p, &list);
    let insts = tree.instances();
    m.placements = insts.len();
    let Some(first) = insts.iter().find(|i| i.leaf).or_else(|| insts.first()) else {
        m.error = Some("the design holds no placement".into());
        return m;
    };
    let path: Vec<Json> = first.path.iter().map(|x| Json::from(*x as f64)).collect();
    match s.op("occurrence_props", obj! {"occurrence" => *first.path.last().unwrap_or(&0) as f64, "path" => Json::Arr(path), "descendants" => true}) {
        Ok(j) => m.geo = Some(Geo::from_props(s.p, &j)),
        Err(e) => m.error = Some(e),
    }
    m
}

/// The geometry of a part's STEP read straight from the file: its vertices in mm (SolidCraft is
/// not involved). They lie on the solid, so their box is inside the solid's box.
pub fn step_vertices(path: &std::path::Path) -> Result<Vec<V3>, String> {
    let model = lcw_step::StepModel::read(path).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for root in &model.roots {
        for (prod, m, _) in model.instances(*root) {
            for s in &model.products[prod].solids {
                out.extend(s.vertices().into_iter().map(|v| m.point(v)));
            }
        }
    }
    Ok(out)
}

pub fn bbox_corners(b: &Bbox) -> [V3; 8] {
    let mut c = [V3::ZERO; 8];
    for (i, slot) in c.iter_mut().enumerate() {
        *slot = v3(if i & 1 == 0 { b.min.x } else { b.max.x }, if i & 2 == 0 { b.min.y } else { b.max.y }, if i & 4 == 0 { b.min.z } else { b.max.z });
    }
    c
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn a_design_told_apart_by_a_number_has_the_same_root() {
        assert_eq!(design_root("lever (2):1"), "lever");
        assert_eq!(design_root("lever (2)"), "lever");
        assert_eq!(design_root("lever:3"), "lever");
        // Inventor's own suffixes are part of the name.
        assert_eq!(design_root("M4 x 20(2):1"), "M4 x 20(2)");
        assert_eq!(design_root("plate (left)"), "plate (left)");
    }
}
