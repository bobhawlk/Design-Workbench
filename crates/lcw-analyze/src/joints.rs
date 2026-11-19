//! How the parts are held together, read off the geometry first, for every assembly whatever
//! its source, with Inventor's constraints and joints as evidence.
//!
//! Inventor's constraints say *which* parts the designer related and *how* (an insert on an
//! axis, a flush or mate of two faces, an angle), but the exported geometry of a constraint is
//! often missing (a plane with no normal, an edge with no centre). A constraint becomes a
//! SolidCraft constraint only when its geometry is complete and lands on the parts' faces;
//! otherwise it is evidence, and nothing is made from it that could lock or free parts wrongly.
//!
//! Inference, over every placed part:
//! 1. Two parts are *pinned* on an axis where a hole of one lines up with a hole or pin of the
//!    other (coaxial, radii within a clearance fit, overlapping or touching along the axis).
//!    Where Inventor inserted two parts and the strict test finds no shared axis, a looser fit is
//!    tried for that pair.
//! 2. The parts grounded in the top assembly (and the parts of grounded sub-assemblies) are the
//!    ground. Parts grounded inside a sub-assembly are fixed only to that sub-assembly in
//!    Inventor, so they are not grounded in the world.
//! 3. Fasteners (screws, nuts, washers…) go rigidly with the part they pass through, and parts
//!    that are round about a single axis (spacers, bushes, shafts) with their biggest neighbour.
//! 4. Parts pinned to each other on two or more different axes are rigid together; repeat on the
//!    rigid groups until nothing changes.
//! 5. A group pinned on one axis turns there when it hangs off the structure and either its name
//!    says it moves (lever, arm, knob…) or Inventor gave it an angle constraint (the designer set
//!    its angle, so it turns). Anything else riding on one pin is held to what it rides on.
//! 6. Groups reached from the ground through pins get revolute joints. Any other group is held
//!    rigidly to the group Inventor constrained it to (so a part mated to a lever moves with the
//!    lever), or to the ground when there is no such evidence. Nothing is left floating.

use std::collections::{BTreeMap, BTreeSet};

use lcw_bundle::{Constraint, EntityKind, EntityRef};
use lcw_geom::{Axis, Bbox, M4, V3};

use crate::features::{Flat, PartFeatures, Round};
use crate::model::{Inst, Model};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Rigid,
    Revolute,
    Slider,
    Cylindrical,
    Planar,
    Ball,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Rigid => "rigid",
            Kind::Revolute => "revolute",
            Kind::Slider => "slider",
            Kind::Cylindrical => "cylindrical",
            Kind::Planar => "planar",
            Kind::Ball => "ball",
        }
    }
    pub fn from_inventor(k: &str) -> Option<Kind> {
        Some(match k {
            "rigid" => Kind::Rigid,
            "rotational" | "revolute" => Kind::Revolute,
            "slider" | "translational" => Kind::Slider,
            "cylindrical" => Kind::Cylindrical,
            "planar" => Kind::Planar,
            "ball" | "spherical" => Kind::Ball,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    InventorJoint,
    InventorConstraint,
    /// Read off the geometry, with a reason.
    Inferred(String),
}

/// A joint between two placed parts, with its frame in world (root assembly) coordinates.
#[derive(Debug, Clone)]
pub struct JointPlan {
    pub name: String,
    pub kind: Kind,
    /// Instance paths (occurrence indices from the root).
    pub a: Vec<usize>,
    pub b: Vec<usize>,
    pub origin: V3,
    pub z: V3,
    pub x: Option<V3>,
    /// Degrees or mm, per value.
    pub limits: Vec<[f64; 2]>,
    pub source: Source,
    /// The Inventor constraints that agree with it (names), for an inferred joint.
    pub evidence: Vec<String>,
}

impl JointPlan {
    /// Where it came from, in words: geometry, Inventor, or both.
    pub fn basis(&self) -> String {
        match &self.source {
            Source::InventorJoint => "Inventor joint".to_string(),
            Source::InventorConstraint => "Inventor constraint".to_string(),
            Source::Inferred(r) if self.evidence.is_empty() => format!("geometry: {r}"),
            Source::Inferred(r) => format!("geometry and Inventor ({}): {r}", short_list(&self.evidence, 4)),
        }
    }
}

/// How a constraint names its geometry in SolidCraft.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// A planar face through this point.
    Face(V3),
    /// A round edge through this point.
    Circle(V3),
    /// A frame: a point and an axis (for axes without a round edge, and points).
    Frame { point: V3, z: V3 },
}

#[derive(Debug, Clone)]
pub struct RefPlan {
    pub path: Vec<usize>,
    pub pick: Pick,
}

/// An Inventor assembly constraint, as SolidCraft's `assembly.constraint.create` rule.
#[derive(Debug, Clone)]
pub struct ConstraintPlan {
    pub name: String,
    /// mate, flush, distance, concentric, insert, parallel, perpendicular, angle, tangent, point_on_axis.
    pub rule: String,
    pub a: RefPlan,
    pub b: RefPlan,
    pub offset: Option<f64>,
    pub angle_deg: Option<f64>,
    pub flip: bool,
}

/// Parts held rigidly to the first (the anchor), each with why.
#[derive(Debug, Clone, Default)]
pub struct RigidGroup {
    pub members: Vec<Vec<usize>>,
    /// One per member: why it is in the group (the anchor's says what the group is).
    pub why: Vec<String>,
}

/// An Inventor constraint or joint that is used as evidence, not carried over as geometry.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub name: String,
    /// The assembly that holds it.
    pub assembly: String,
    /// insert, flush, mate, angle… (`joint:<type>` for an Inventor joint).
    pub kind: String,
    /// Instance paths of the two sides: a part, or a sub-assembly.
    pub a: Vec<usize>,
    pub b: Vec<usize>,
    /// Why it is not a SolidCraft constraint.
    pub why_not: String,
    /// What it agreed with, or what it did, once the joints were worked out.
    pub outcome: String,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The grounded instances (in the world: the top assembly's grounded occurrences).
    pub ground: Vec<Vec<usize>>,
    pub joints: Vec<JointPlan>,
    pub constraints: Vec<ConstraintPlan>,
    /// Groups of instances held rigidly together, from inference.
    pub rigid_groups: Vec<RigidGroup>,
    /// Inventor constraints and joints used as evidence only.
    pub evidence: Vec<Evidence>,
    pub notes: Vec<String>,
}

pub struct Options {
    /// Infer joints from the geometry.
    pub infer: bool,
    /// Coaxial tolerance (mm).
    pub axis_tol: f64,
    /// Largest radius difference for two round faces to count as a fit (mm).
    pub fit: f64,
    /// The looser coaxial tolerance and fit tried where Inventor inserted two parts (mm).
    pub evidence_axis_tol: f64,
    pub evidence_fit: f64,
    /// How far a constraint's picked point may be from the face it names (mm).
    pub pick_tol: f64,
    /// Name fragments that make a part a fastener (lowercase).
    pub fastener_words: Vec<String>,
    /// Name fragments of parts that turn when pinned to the structure on one axis.
    pub motion_words: Vec<String>,
    /// Name fragments of parts that never turn on their own (they win over `motion_words`).
    pub fixed_words: Vec<String>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            infer: true,
            axis_tol: 0.15,
            fit: 0.75,
            evidence_axis_tol: 0.6,
            evidence_fit: 2.0,
            pick_tol: 0.05,
            fastener_words: [
                "bolt", "screw", "nut", "washer", "nyloc", "rivet", "standoff", "stand-off", "circlip", "dowel", "bs 4168", "iso 4762", "iso 4032", "iso 7089",
                "din 912", "din 934", "din 125", "ansi", "asme b18", "as 1421", "metric series", "socket head", "cap screw", "hex head", "m2 x", "m2.5 x", "m3 x",
                "m4 x", "m5 x", "m6 x", "m8 x", "m10 x",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            motion_words: [
                "lever", "arm", "link", "crank", "knob", "wheel", "drum", "rotor", "gear", "pulley", "sprocket", "flap", "door", "lid", "hinge", "pedal", "rocker",
                "cam", "dial", "tiller", "trigger", "toggle", "throttle", "yoke", "paddle", "tab", "swing", "pivot",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            fixed_words: ["handle", "grip", "cap", "cover", "bezel", "ring", "plate", "bracket", "mount", "spacer", "washer", "bush", "sleeve", "label", "sticker", "aluminium for"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Map Inventor's constraint kinds onto SolidCraft's rules.
pub fn rule_for(kind: &str, a: EntityKind, b: EntityKind) -> Option<&'static str> {
    Some(match kind {
        "mate" if a.is_axial() && b.is_axial() => "concentric",
        "mate" if a == EntityKind::Point || b == EntityKind::Point => "point_on_axis",
        "mate" => "mate",
        "flush" => "flush",
        "angle" => "angle",
        "insert" => "insert",
        "tangent" => "tangent",
        "concentric" => "concentric",
        "parallel" => "parallel",
        "perpendicular" => "perpendicular",
        _ => return None,
    })
}

fn short_list(names: &[String], n: usize) -> String {
    let mut s = names.iter().take(n).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > n {
        s.push_str(&format!(" and {} more", names.len() - n));
    }
    s
}

/// One placed part's faces in world coordinates, for checking a constraint's picks.
struct PlacedFaces {
    rounds: Vec<Round>,
    flats: Vec<Flat>,
}

fn placed_faces(f: &PartFeatures, world: &M4) -> PlacedFaces {
    PlacedFaces {
        rounds: f.rounds.iter().map(|r| r.transformed(world)).collect(),
        flats: f.flats.iter().map(|x| Flat { origin: world.point(x.origin), normal: world.dir(x.normal), area: x.area }).collect(),
    }
}

/// The SolidCraft pick for one side of an Inventor constraint, when its exported geometry is
/// complete and lands on a face of the part it names; otherwise why not.
fn checked_pick(e: &EntityRef, to_world: &M4, faces: Option<&PlacedFaces>, tol: f64) -> Result<Pick, String> {
    let Some(faces) = faces else { return Err("it names a sub-assembly or a part with no solid, not a face of a part".into()) };
    // The exporter writes (0, 0, 0) and no direction when it could not read the geometry.
    let placeholder = e.point == V3::ZERO && e.dir.is_none() && e.rim_point.is_none();
    match e.kind {
        EntityKind::Other => Err("its geometry kind was not exported".into()),
        _ if placeholder => Err(format!("its {} has no exported geometry (point (0, 0, 0), no direction)", e.kind.name())),
        EntityKind::Plane => {
            let Some(n) = e.dir.and_then(|d| to_world.dir(d).unit()) else { return Err("its plane has no normal in the export".into()) };
            let p = to_world.point(e.point);
            if faces.flats.iter().any(|f| f.normal.dot(n).abs() > 0.999 && (p - f.origin).dot(f.normal).abs() < tol) {
                Ok(Pick::Face(p))
            } else {
                Err("no planar face of the part lies on its plane".into())
            }
        }
        EntityKind::Cylinder | EntityKind::Cone | EntityKind::Edge => {
            let Some(rim) = e.rim_point.map(|r| to_world.point(r)) else { return Err(format!("its {} has no point on a round edge in the export", e.kind.name())) };
            let along = |r: &Round, p: V3| (p - r.axis.origin).dot(r.axis.dir);
            let on_circle = |r: &Round| (r.axis.distance_to(rim) - r.radius).abs() < tol && along(r, rim) > r.lo - tol && along(r, rim) < r.hi + tol;
            if faces.rounds.iter().any(|r| on_circle(r) && r.covers(rim, tol)) {
                return Ok(Pick::Circle(rim));
            }
            // On the circle of a face that doesn't go all the way round (the arc of a slot), but
            // off the arc: the exporter's rim point is the circle's, not the edge's. The same
            // circle, picked on the arc at the same place along the axis (on a flat wall of the
            // slot SolidCraft finds no round face).
            if let Some(r) = faces.rounds.iter().find(|r| on_circle(r)) {
                return Ok(Pick::Circle(r.point_at(along(r, rim), rim)));
            }
            // An edge of a different radius on the same axis (a counterbore's step, a cone): the
            // round face on that axis, picked in the same plane (SolidCraft's insert takes round faces).
            let axis = e.dir.and_then(|d| Axis::new(to_world.point(e.point), to_world.dir(d)));
            let coaxial = axis.and_then(|ax| {
                faces.rounds.iter().filter(|r| r.axis.coincident(&ax, tol.max(0.01) * 4.0)).min_by(|x, y| {
                    let gap = |r: &Round| {
                        let t = along(r, rim);
                        if t < r.lo { r.lo - t } else if t > r.hi { t - r.hi } else { 0.0 }
                    };
                    gap(x).total_cmp(&gap(y))
                })
            });
            match coaxial {
                // Only across the axis: picking another end of the face would move the insert's
                // plane, and SolidCraft would then place the part off where Inventor has it.
                Some(r) if (along(r, rim).clamp(r.lo, r.hi) - along(r, rim)).abs() > tol => {
                    Err(format!("its circle is {:.2} mm along the axis from the nearest round face of the part (a chamfer or countersink edge?)", (along(r, rim).clamp(r.lo, r.hi) - along(r, rim)).abs()))
                }
                Some(r) => Ok(Pick::Circle(r.point_at(along(r, rim).clamp(r.lo, r.hi), rim))),
                None => Err("no round face of the part passes through its rim point or lies on its axis".into()),
            }
        }
        EntityKind::Axis => match e.dir.and_then(|d| to_world.dir(d).unit()) {
            Some(z) => Ok(Pick::Frame { point: to_world.point(e.point), z }),
            None => Err("its axis has no direction in the export".into()),
        },
        EntityKind::Point | EntityKind::Sphere => Ok(Pick::Frame { point: to_world.point(e.point), z: e.dir.map(|d| to_world.dir(d)).unwrap_or(V3::Z) }),
    }
}

/// Everything about one placed part the inference needs.
struct Body {
    inst: usize,
    name: String,
    rounds: Vec<Round>,
    bbox: Bbox,
    size: f64,
    fastener: bool,
    axisym: bool,
}

/// The distinct axes two parts share: a hole of one on a hole or pin of the other, coaxial
/// within `tol`, radii within `fit` (or a tenth of the smaller), overlapping along the axis
/// within `gap`.
fn shared_axes(a: &Body, b: &Body, tol: f64, fit: f64, gap: f64) -> Vec<Axis> {
    let mut axes: Vec<Axis> = Vec::new();
    for ra in &a.rounds {
        for rb in &b.rounds {
            if !(ra.hole || rb.hole) || (ra.radius - rb.radius).abs() > fit.max(0.1 * ra.radius.min(rb.radius)) {
                continue;
            }
            if !ra.axis.coincident(&rb.axis, tol) {
                continue;
            }
            // Along the axis, in a's parameter.
            let (alo, ahi) = (ra.lo, ra.hi);
            let shift = (rb.axis.origin - ra.axis.origin).dot(ra.axis.dir);
            let s = if rb.axis.dir.dot(ra.axis.dir) > 0.0 { 1.0 } else { -1.0 };
            let (b1, b2) = (shift + s * rb.lo, shift + s * rb.hi);
            let (blo, bhi) = (b1.min(b2), b1.max(b2));
            if blo > ahi + gap || alo > bhi + gap {
                continue;
            }
            if !axes.iter().any(|x| x.coincident(&ra.axis, tol * 2.0)) {
                axes.push(ra.axis.normalised());
            }
        }
    }
    axes
}

fn find(p: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while p[r] != r {
        r = p[r];
    }
    let mut k = i;
    while p[k] != r {
        let next = p[k];
        p[k] = r;
        k = next;
    }
    r
}

/// Merge groups pinned to each other on two or more distinct axes, until nothing changes.
fn merge_two_axis(parent: &mut [usize], shared: &BTreeMap<(usize, usize), Vec<Axis>>, tol: f64, reason: &mut BTreeMap<usize, String>) {
    loop {
        let mut by_pair: BTreeMap<(usize, usize), Vec<Axis>> = BTreeMap::new();
        for ((a, b), axes) in shared {
            let (ra, rb) = (find(parent, *a), find(parent, *b));
            if ra == rb {
                continue;
            }
            let list = by_pair.entry((ra.min(rb), ra.max(rb))).or_default();
            for ax in axes {
                if !list.iter().any(|x| x.coincident(ax, tol * 2.0)) {
                    list.push(*ax);
                }
            }
        }
        let merge: Vec<(usize, usize)> = by_pair.iter().filter(|(_, ax)| ax.len() >= 2).map(|(k, _)| *k).collect();
        if merge.is_empty() {
            break;
        }
        for (a, b) in merge {
            let (ra, rb) = (find(parent, a), find(parent, b));
            if ra != rb {
                parent[ra] = rb;
                reason.entry(ra).or_insert_with(|| "pinned on two or more axes".into());
            }
        }
    }
}

pub fn plan(model: &Model, instances: &[Inst], features: &[Option<PartFeatures>], opts: &Options) -> Plan {
    let mut out = Plan::default();
    let inst_index: BTreeMap<Vec<usize>, usize> = instances.iter().enumerate().map(|(i, x)| (x.path.clone(), i)).collect();
    // Each placed part's faces in the world, for checking constraint picks.
    let faces_of = |path: &[usize]| -> Option<PlacedFaces> {
        let x = &instances[*inst_index.get(path)?];
        let f = features[x.def].as_ref()?;
        Some(placed_faces(f, &x.world))
    };
    // Assembly instances (and the root) by path, for constraints in sub-assemblies.
    let mut asm_places: Vec<(usize, Vec<usize>, M4)> = vec![(model.root, Vec::new(), M4::IDENTITY)];
    for x in instances {
        if model.defs[x.def].assembly {
            asm_places.push((x.def, x.path.clone(), x.world));
        }
    }
    // Parts an Inventor joint already holds: inference leaves them.
    let mut constrained: BTreeSet<Vec<usize>> = BTreeSet::new();
    let mut sub_grounds: Vec<String> = Vec::new();
    for (def, base, world) in &asm_places {
        let d = &model.defs[*def];
        for c in d.constraints.iter().filter(|c| !c.suppressed) {
            let (Some(ka), Some(kb)) = (model.resolve(*def, &c.a.path), model.resolve(*def, &c.b.path)) else {
                out.notes.push(format!("{} › {}: a side names an occurrence that is not in the assembly; left out", d.name, c.name));
                continue;
            };
            let a: Vec<usize> = base.iter().copied().chain(ka).collect();
            let b: Vec<usize> = base.iter().copied().chain(kb).collect();
            let mut ev = Evidence { name: c.name.clone(), assembly: d.name.clone(), kind: c.kind.clone(), a: a.clone(), b: b.clone(), why_not: String::new(), outcome: String::new() };
            if a.len() == base.len() || b.len() == base.len() {
                ev.why_not = "one side is the assembly's own geometry (an origin plane or axis), not a part".into();
                out.evidence.push(ev);
                continue;
            }
            if a == b || a.starts_with(&b) || b.starts_with(&a) {
                ev.why_not = "both sides are on the same occurrence".into();
                out.evidence.push(ev);
                continue;
            }
            // A side the export names by its sub-assembly: the part below it the geometry lands on.
            let pick_on = |e: &EntityRef, path: &Vec<usize>| -> Result<(Vec<usize>, Pick), String> {
                let direct = checked_pick(e, world, faces_of(path).as_ref(), opts.pick_tol);
                if direct.is_ok() || !inst_index.get(path).is_some_and(|i| model.defs[instances[*i].def].assembly) {
                    return direct.map(|p| (path.clone(), p));
                }
                let leaves: Vec<&Inst> = instances.iter().filter(|x| x.path.len() > path.len() && x.path.starts_with(path) && !model.defs[x.def].assembly).collect();
                let hits: Vec<(Vec<usize>, Pick)> = leaves.iter().filter_map(|x| checked_pick(e, world, faces_of(&x.path).as_ref(), opts.pick_tol).ok().map(|p| (x.path.clone(), p))).collect();
                match hits.len() {
                    1 => hits.into_iter().next().ok_or_else(String::new),
                    0 => Err(format!("it names a sub-assembly, and its geometry lands on none of the {} part(s) in it", leaves.len())),
                    k => Err(format!("it names a sub-assembly, and its geometry lands on {k} of the parts in it")),
                }
            };
            let usable = (|| -> Result<ConstraintPlan, String> {
                let (pa_path, pa) = pick_on(&c.a, &a).map_err(|e| format!("first side: {e}"))?;
                let (pb_path, pb) = pick_on(&c.b, &b).map_err(|e| format!("second side: {e}"))?;
                if pa_path == pb_path {
                    return Err("both sides land on the same part".into());
                }
                constraint_plan(c, RefPlan { path: pa_path, pick: pa }, RefPlan { path: pb_path, pick: pb })
            })();
            match usable {
                // The inference still covers these parts: the constraints hold them as Inventor
                // did, and the inferred holds and joints agree with them as built, so a group of
                // constrained parts is never left joined to nothing grounded.
                Ok(p) => out.constraints.push(p),
                Err(why) => {
                    ev.why_not = why;
                    out.evidence.push(ev);
                }
            }
        }
        for j in d.joints.iter().filter(|j| !j.suppressed) {
            let (Some(ka), Some(kb)) = (model.resolve(*def, &j.a.path), model.resolve(*def, &j.b.path)) else {
                out.notes.push(format!("{} › {}: a side names an occurrence that is not in the assembly", d.name, j.name));
                continue;
            };
            let a: Vec<usize> = base.iter().copied().chain(ka).collect();
            let b: Vec<usize> = base.iter().copied().chain(kb).collect();
            let kind = Kind::from_inventor(&j.kind);
            // The exporter writes (0, 0, 0) along +Z when it could not read a joint's origin.
            let placeholder = j.b.origin == V3::ZERO && j.b.z == V3::Z && j.a.origin == V3::ZERO;
            let why_not = match kind {
                None => Some(format!("Inventor joint type `{}` has no SolidCraft equivalent", j.kind)),
                Some(_) if placeholder => Some("its origin was not exported ((0, 0, 0) along +Z)".to_string()),
                Some(_) if a == b => Some("both sides are on the same occurrence".to_string()),
                _ => None,
            };
            if let (Some(kind), None) = (kind, &why_not) {
                constrained.insert(a.clone());
                constrained.insert(b.clone());
                out.joints.push(JointPlan {
                    name: j.name.clone(),
                    kind,
                    a,
                    b,
                    origin: world.point(j.b.origin),
                    z: world.dir(j.b.z),
                    x: j.b.x.map(|x| world.dir(x)),
                    limits: j.limits.clone(),
                    source: Source::InventorJoint,
                    evidence: Vec::new(),
                });
            } else {
                out.evidence.push(Evidence { name: j.name.clone(), assembly: d.name.clone(), kind: format!("joint:{}", j.kind), a, b, why_not: why_not.unwrap_or_default(), outcome: String::new() });
            }
        }
        for (i, o) in d.occurrences.iter().enumerate() {
            if o.grounded && !o.suppressed {
                let mut p = base.clone();
                p.push(i);
                if base.is_empty() {
                    out.ground.push(p);
                } else {
                    sub_grounds.push(format!("{} (in {})", o.id, d.name));
                }
            }
        }
    }
    if !sub_grounds.is_empty() {
        out.notes.push(format!(
            "grounded inside a sub-assembly, so fixed only to that sub-assembly in Inventor and not grounded here (the geometry decides what holds them): {}",
            short_list(&sub_grounds, 12)
        ));
    }
    if !out.evidence.is_empty() {
        let mut by_why: BTreeMap<String, usize> = BTreeMap::new();
        for e in &out.evidence {
            let w = e.why_not.split(": ").last().unwrap_or("").to_string();
            *by_why.entry(w).or_default() += 1;
        }
        let whys: Vec<String> = by_why.iter().map(|(w, n)| format!("{n}: {w}")).collect();
        out.notes.push(format!(
            "{} Inventor constraint(s) and joint(s) used as evidence only, not as SolidCraft constraints ({}); {} carried over as SolidCraft constraints",
            out.evidence.len(),
            whys.join("; "),
            out.constraints.len()
        ));
    }
    if !opts.infer {
        return out;
    }
    // Leaf parts with geometry take part in the inference.
    let bodies: Vec<Body> = instances
        .iter()
        .enumerate()
        .filter(|(_, x)| !model.defs[x.def].assembly)
        .filter_map(|(i, x)| {
            let f = features[x.def].as_ref()?;
            let bbox = Bbox::of(f.vertices.iter().map(|p| x.world.point(*p)));
            if bbox.is_empty() {
                return None;
            }
            let name = model.defs[x.def].name.to_lowercase();
            let pf = placed_faces(f, &x.world);
            Some(Body {
                inst: i,
                name: name.clone(),
                rounds: pf.rounds,
                size: bbox.diagonal(),
                bbox,
                fastener: opts.fastener_words.iter().any(|w| name.contains(w.as_str())),
                axisym: f.axisymmetric(),
            })
        })
        .collect();
    let n = bodies.len();
    if n == 0 {
        return out;
    }
    let path_of = |i: usize| -> &Vec<usize> { &instances[bodies[i].inst].path };
    let def_name = |i: usize| -> &str { &model.defs[instances[bodies[i].inst].def].name };
    // The bodies on each side of each piece of evidence (a sub-assembly side: all its parts).
    let under = |p: &[usize]| -> Vec<usize> { (0..n).filter(|i| path_of(*i).starts_with(p)).collect() };
    let ev_bodies: Vec<(Vec<usize>, Vec<usize>)> = out.evidence.iter().map(|e| (under(&e.a), under(&e.b))).collect();
    let is_insert = |k: &str| matches!(k, "insert" | "joint:rotational" | "joint:revolute" | "joint:cylindrical");
    // 1. Pins between pairs: the distinct shared axes.
    let mut shared: BTreeMap<(usize, usize), Vec<Axis>> = BTreeMap::new();
    for a in 0..n {
        for b in a + 1..n {
            if !overlap(&bodies[a].bbox, &bodies[b].bbox, 1.0) {
                continue;
            }
            let axes = shared_axes(&bodies[a], &bodies[b], opts.axis_tol, opts.fit, 1.0);
            if !axes.is_empty() {
                shared.insert((a, b), axes);
            }
        }
    }
    // Inventor's inserts: confirm pins, and look again with a looser fit where none was found.
    let mut pin_evidence: BTreeMap<(usize, usize), Vec<String>> = BTreeMap::new();
    let mut relaxed = 0usize;
    for (k, (ba, bb)) in ev_bodies.iter().enumerate() {
        if !is_insert(&out.evidence[k].kind) {
            continue;
        }
        for &x in ba {
            for &y in bb {
                if x == y {
                    continue;
                }
                let key = (x.min(y), x.max(y));
                if !shared.contains_key(&key) && ba.len() * bb.len() <= 64 && overlap(&bodies[x].bbox, &bodies[y].bbox, 2.0) {
                    let axes = shared_axes(&bodies[key.0], &bodies[key.1], opts.evidence_axis_tol, opts.evidence_fit, 3.0);
                    if !axes.is_empty() {
                        shared.insert(key, axes);
                        relaxed += 1;
                    }
                }
                if shared.contains_key(&key) {
                    let list = pin_evidence.entry(key).or_default();
                    if !list.contains(&out.evidence[k].name) {
                        list.push(out.evidence[k].name.clone());
                    }
                }
            }
        }
    }
    let mut parent: Vec<usize> = (0..n).collect();
    let mut reason: BTreeMap<usize, String> = BTreeMap::new();
    // 2. The ground: parts grounded in the top assembly, and the parts of grounded sub-assemblies.
    let seeds: Vec<usize> = (0..n).filter(|i| out.ground.iter().any(|g| path_of(*i).starts_with(g))).collect();
    for w in seeds.windows(2) {
        let (r0, r1) = (find(&mut parent, w[0]), find(&mut parent, w[1]));
        if r0 != r1 {
            parent[r1] = r0;
        }
    }
    for s in &seeds {
        reason.insert(*s, "grounded in Inventor".into());
    }
    // 3. Fasteners go with a neighbour.
    let neighbours = |i: usize| -> Vec<(usize, usize)> {
        shared.iter().filter_map(|((a, b), ax)| if *a == i { Some((*b, ax.len())) } else if *b == i { Some((*a, ax.len())) } else { None }).collect()
    };
    for i in 0..n {
        if bodies[i].fastener && !seeds.contains(&i) {
            let host = neighbours(i).into_iter().filter(|(j, _)| !bodies[*j].fastener).max_by(|a, b| bodies[a.0].size.total_cmp(&bodies[b.0].size));
            if let Some((j, _)) = host {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
                reason.insert(i, format!("fastener in `{}`", def_name(j)));
            }
        }
    }
    // 4. Two or more axes between groups make them rigid.
    merge_two_axis(&mut parent, &shared, opts.axis_tol, &mut reason);
    // The ground group: the grounded parts', else the biggest.
    let grounded_body = seeds.first().copied();
    let ground_root = match grounded_body {
        Some(g) => find(&mut parent, g),
        None => {
            let mut volume: BTreeMap<usize, f64> = BTreeMap::new();
            for i in 0..n {
                let r = find(&mut parent, i);
                let s = bodies[i].bbox.size();
                *volume.entry(r).or_default() += s.x * s.y * s.z;
            }
            volume.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|x| x.0).unwrap_or(0)
        }
    };
    // Round parts (spacers, bushes): with the ground when they touch it, else their biggest neighbour.
    for i in 0..n {
        if !bodies[i].axisym || bodies[i].fastener || seeds.contains(&i) || find(&mut parent, i) != i || parent.iter().filter(|p| **p == i).count() > 1 {
            continue;
        }
        let nb = neighbours(i);
        if nb.is_empty() {
            continue;
        }
        let to_ground = nb.iter().find(|(j, _)| find(&mut parent.clone(), *j) == ground_root).map(|x| x.0);
        let host = to_ground.or_else(|| nb.iter().max_by(|a, b| bodies[a.0].size.total_cmp(&bodies[b.0].size)).map(|x| x.0));
        if let Some(j) = host {
            let rj = find(&mut parent, j);
            parent[i] = rj;
            reason.insert(i, format!("round part on `{}`", def_name(j)));
        }
    }
    merge_two_axis(&mut parent, &shared, opts.axis_tol, &mut reason);
    let seed = match grounded_body {
        Some(g) => g,
        None => {
            let snapshot = parent.clone();
            (0..n).find(|i| find(&mut snapshot.clone(), *i) == ground_root).unwrap_or(0)
        }
    };
    // Inventor evidence about a group: the constraints from its members to parts outside it.
    let outward = |parent: &mut Vec<usize>, members: &[usize]| -> Vec<usize> {
        let g = find(parent, members[0]);
        (0..out.evidence.len())
            .filter(|k| {
                let (ba, bb) = &ev_bodies[*k];
                // One side wholly in the group (a part of it, or a sub-assembly it holds), the
                // other wholly outside.
                let all_in = |s: &[usize], p: &mut Vec<usize>| !s.is_empty() && s.iter().all(|x| find(p, *x) == g);
                let none_in = |s: &[usize], p: &mut Vec<usize>| !s.is_empty() && s.iter().all(|x| find(p, *x) != g);
                (all_in(ba, parent) && none_in(bb, parent)) || (all_in(bb, parent) && none_in(ba, parent))
            })
            .collect()
    };
    // 5. A group pinned on one axis only turns when it hangs off the structure and its name or an
    // Inventor angle constraint says it moves; anything else on one pin is held to what it rides on.
    let says = |members: &[usize], words: &[String]| members.iter().any(|m| words.iter().any(|w| bodies[*m].name.contains(w.as_str())));
    let mut moving_why: BTreeMap<usize, String> = BTreeMap::new();
    loop {
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in 0..n {
            let r = find(&mut parent, i);
            members.entry(r).or_default().push(i);
        }
        for ms in members.values_mut() {
            ms.sort_by(|a, b| bodies[*b].size.total_cmp(&bodies[*a].size));
        }
        let ground_now = find(&mut parent, seed);
        // Each group's pins: (other group, axis).
        let mut pins_of: BTreeMap<usize, Vec<(usize, Axis)>> = BTreeMap::new();
        for ((a, b), axes) in &shared {
            let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
            if ra == rb {
                continue;
            }
            for ax in axes {
                pins_of.entry(ra).or_default().push((rb, *ax));
                pins_of.entry(rb).or_default().push((ra, *ax));
            }
        }
        let mut changed = false;
        for (g, pins) in &pins_of {
            if *g == ground_now {
                continue;
            }
            let mut axes: Vec<Axis> = Vec::new();
            for (_, ax) in pins {
                if !axes.iter().any(|x| x.coincident(ax, opts.axis_tol * 2.0)) {
                    axes.push(*ax);
                }
            }
            if axes.len() != 1 {
                continue;
            }
            let ms = members.get(g).cloned().unwrap_or_default();
            let on_ground = pins.iter().any(|(o, _)| *o == ground_now);
            let anchor_fixed = says(&ms[..1.min(ms.len())], &opts.fixed_words);
            let by_name = says(&ms, &opts.motion_words) && !anchor_fixed;
            let angles: Vec<String> = outward(&mut parent, &ms).into_iter().filter(|k| out.evidence[*k].kind == "angle").map(|k| out.evidence[k].name.clone()).collect();
            let by_angle = !angles.is_empty() && !ms.iter().all(|m| bodies[*m].fastener || bodies[*m].axisym);
            if on_ground && (by_name || by_angle) {
                let why = match (by_name, by_angle) {
                    (true, true) => format!("its name says it moves and Inventor sets its angle ({})", short_list(&angles, 3)),
                    (true, false) => "its name says it moves".to_string(),
                    _ => format!("Inventor sets its angle ({})", short_list(&angles, 3)),
                };
                moving_why.insert(*g, why);
                continue;
            }
            // Held to the biggest group it is pinned to (the ground when it is one of them).
            let host = if on_ground {
                ground_now
            } else {
                pins.iter().map(|(o, _)| *o).max_by(|x, y| {
                    let sx: f64 = members.get(x).map_or(0.0, |m| m.iter().map(|i| bodies[*i].size).sum());
                    let sy: f64 = members.get(y).map_or(0.0, |m| m.iter().map(|i| bodies[*i].size).sum());
                    sx.total_cmp(&sy)
                }).unwrap_or(ground_now)
            };
            let host_name = members.get(&host).and_then(|m| m.first()).map(|i| def_name(*i).to_string()).unwrap_or_default();
            for m in &ms {
                reason.entry(*m).or_insert_with(|| format!("on one pin in `{host_name}`, with nothing saying it moves"));
            }
            let rg = find(&mut parent, *g);
            let rh = find(&mut parent, host);
            if rg != rh {
                parent[rg] = rh;
                moving_why.remove(&rg);
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    // Groups and their members, biggest member first (a grounded part first in the ground's).
    let ground_root = find(&mut parent, seed);
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    for members in groups.values_mut() {
        members.sort_by(|a, b| seeds.contains(b).cmp(&seeds.contains(a)).then(bodies[*b].size.total_cmp(&bodies[*a].size)));
    }
    let free = |path: &Vec<usize>| !constrained.contains(path);
    // 6. Revolute joints between groups pinned on one axis.
    let mut pins: BTreeMap<(usize, usize), (Axis, usize, usize)> = BTreeMap::new();
    for ((a, b), axes) in &shared {
        let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
        if ra == rb || axes.len() != 1 {
            continue;
        }
        let key = (ra.min(rb), ra.max(rb));
        // Prefer the pair Inventor inserted, then the biggest.
        let score = |x: usize, y: usize| (pin_evidence.contains_key(&(x.min(y), x.max(y))), bodies[x].size + bodies[y].size);
        match pins.get(&key) {
            Some((_, pa, pb)) if score(*pa, *pb).partial_cmp(&score(*a, *b)).is_some_and(|o| o.is_ge()) => {}
            _ => {
                pins.insert(key, (axes[0], *a, *b));
            }
        }
    }
    // Groups reachable from the ground through pins move; the rest are held.
    let mut reach: BTreeSet<usize> = BTreeSet::from([ground_root]);
    loop {
        let before = reach.len();
        for (a, b) in pins.keys() {
            if reach.contains(a) || reach.contains(b) {
                reach.insert(*a);
                reach.insert(*b);
            }
        }
        if reach.len() == before {
            break;
        }
    }
    let ground_anchor = groups.get(&ground_root).and_then(|m| m.first().copied()).unwrap_or(seed);
    if grounded_body.is_none() && free(path_of(ground_anchor)) {
        out.ground.push(path_of(ground_anchor).clone());
        out.notes.push(format!("grounded `{}`, the biggest part of the main structure", def_name(ground_anchor)));
    }
    let mut count = 0usize;
    let mut joint_groups: Vec<(usize, usize, String)> = Vec::new();
    for ((ga, gb), (axis, pa, pb)) in &pins {
        let (pa_path, pb_path) = (path_of(*pa).clone(), path_of(*pb).clone());
        if !free(&pa_path) && !free(&pb_path) {
            continue;
        }
        // a is on the ground's side, b moves.
        let (a, b) = if find(&mut parent, *pb) == ground_root { (pb, pa) } else { (pa, pb) };
        let ra = bodies[*b].rounds.iter().find(|r| r.axis.coincident(axis, opts.axis_tol * 2.0)).or_else(|| bodies[*b].rounds.iter().find(|r| r.axis.coincident(axis, opts.evidence_axis_tol * 2.0)));
        let origin = ra.map_or(axis.origin, |r| r.axis.origin + r.axis.dir * r.axial_mid());
        // The evidence: Inventor's inserts between the two groups, and the angles on the moving one.
        let gm = find(&mut parent, *b);
        let mut evidence: Vec<String> = Vec::new();
        for ((x, y), names) in &pin_evidence {
            let (rx, ry) = (find(&mut parent, *x), find(&mut parent, *y));
            if (rx == *ga && ry == *gb) || (rx == *gb && ry == *ga) {
                for nm in names {
                    if !evidence.contains(nm) {
                        evidence.push(nm.clone());
                    }
                }
            }
        }
        let mut why = format!("pinned on one axis to `{}`", def_name(*a));
        if let Some(m) = moving_why.get(&gm) {
            why.push_str(&format!("; turns because {m}"));
        }
        if pin_evidence.contains_key(&((*pa).min(*pb), (*pa).max(*pb))) && shared_axes(&bodies[(*pa).min(*pb)], &bodies[(*pa).max(*pb)], opts.axis_tol, opts.fit, 1.0).is_empty() {
            why.push_str("; the pin was found with the looser fit tried because Inventor inserted the two parts");
        }
        count += 1;
        let name = format!("{} pivot", def_name(*b));
        joint_groups.push((*ga, *gb, name.clone()));
        out.joints.push(JointPlan {
            name,
            kind: Kind::Revolute,
            a: path_of(*a).clone(),
            b: path_of(*b).clone(),
            origin,
            z: axis.dir,
            x: None,
            limits: Vec::new(),
            source: Source::Inferred(why),
            evidence,
        });
    }
    // Groups with no joint: held to what Inventor constrained them to (a group that is reached,
    // or one already settled), else to the ground. Settle in rounds so holds never form a loop.
    let mut host_of: BTreeMap<usize, (usize, String)> = BTreeMap::new();
    let mut settled: BTreeSet<usize> = reach.clone();
    loop {
        let mut progress = false;
        for (g, members) in &groups {
            if settled.contains(g) {
                continue;
            }
            let ks = outward(&mut parent, members);
            let mut candidates: BTreeMap<usize, Vec<String>> = BTreeMap::new();
            for k in &ks {
                let (ba, bb) = &ev_bodies[*k];
                for x in ba.iter().chain(bb) {
                    let r = find(&mut parent, *x);
                    if r != *g && settled.contains(&r) {
                        let list = candidates.entry(r).or_default();
                        if !list.contains(&out.evidence[*k].name) {
                            list.push(out.evidence[*k].name.clone());
                        }
                    }
                }
            }
            let pick = if candidates.contains_key(&ground_root) {
                candidates.remove_entry(&ground_root)
            } else {
                candidates.into_iter().max_by(|x, y| {
                    let sx: f64 = groups.get(&x.0).map_or(0.0, |m| m.iter().map(|i| bodies[*i].size).sum());
                    let sy: f64 = groups.get(&y.0).map_or(0.0, |m| m.iter().map(|i| bodies[*i].size).sum());
                    sx.total_cmp(&sy)
                })
            };
            if let Some((h, names)) = pick {
                let hn = groups.get(&h).and_then(|m| m.first()).map(|i| def_name(*i).to_string()).unwrap_or_default();
                host_of.insert(*g, (h, format!("held to `{hn}`, which Inventor constrains it to ({})", short_list(&names, 3))));
                settled.insert(*g);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    let mut unreached = 0usize;
    for g in groups.keys() {
        if !settled.contains(g) {
            host_of.insert(*g, (ground_root, "no pin and no Inventor constraint to anything that places it: held to the main structure".into()));
            unreached += 1;
        }
    }
    // Rigid groups, anchor first.
    for (g, members) in &groups {
        let mut list: Vec<(Vec<usize>, String)> = Vec::new();
        let anchor = members[0];
        let mut anchor_why = if *g == ground_root { "the main structure".to_string() } else if moving_why.contains_key(g) { "moves on its pivot".to_string() } else { "a part group".to_string() };
        if let Some((h, why)) = host_of.get(g) {
            let host_anchor = groups.get(h).and_then(|m| m.first().copied()).unwrap_or(ground_anchor);
            list.push((path_of(host_anchor).clone(), "anchor".into()));
            anchor_why = why.clone();
        }
        list.push((path_of(anchor).clone(), anchor_why));
        for m in &members[1..] {
            let why = reason.get(m).cloned().or_else(|| reason.get(&find(&mut parent.clone(), *m)).cloned()).unwrap_or_else(|| "with its group".into());
            list.push((path_of(*m).clone(), why));
        }
        // Grounded parts are fixed already (only the anchor may be one), and parts a SolidCraft
        // constraint holds are left to it.
        let head = list.remove(0);
        let rest: Vec<(Vec<usize>, String)> = list.into_iter().filter(|(p, _)| free(p) && !out.ground.contains(p)).collect();
        if !rest.is_empty() {
            let mut rg = RigidGroup { members: vec![head.0], why: vec![head.1] };
            for (p, w) in rest {
                rg.members.push(p);
                rg.why.push(w);
            }
            out.rigid_groups.push(rg);
        }
    }
    // What became of each piece of evidence.
    for (k, (ba, bb)) in ev_bodies.iter().enumerate() {
        let e = &mut out.evidence[k];
        if ba.is_empty() || bb.is_empty() {
            e.outcome = "names no part with geometry".into();
            continue;
        }
        let ga: BTreeSet<usize> = ba.iter().map(|x| find(&mut parent, *x)).collect();
        let gb: BTreeSet<usize> = bb.iter().map(|x| find(&mut parent, *x)).collect();
        let anchor_name = |g: usize| groups.get(&g).and_then(|m| m.first()).map(|i| def_name(*i).to_string()).unwrap_or_default();
        if let Some(g) = ga.intersection(&gb).next() {
            e.outcome = format!("agrees: the parts are held rigidly together (with `{}`)", anchor_name(*g));
        } else if let Some((_, _, j)) = joint_groups.iter().find(|(x, y, _)| (ga.contains(x) && gb.contains(y)) || (ga.contains(y) && gb.contains(x))) {
            e.outcome = format!("agrees: `{j}` joins them");
        } else if ga.iter().chain(&gb).any(|g| host_of.get(g).is_some_and(|(h, _)| ga.contains(h) || gb.contains(h))) {
            e.outcome = "used: it decided what a part with no joint is held to".into();
        } else {
            let names: Vec<String> = ga.iter().chain(&gb).take(2).map(|g| anchor_name(*g)).collect();
            e.outcome = format!("not used: the geometry puts its parts in groups that are not joined directly ({})", names.join(" / "));
        }
    }
    if relaxed > 0 {
        out.notes.push(format!("{relaxed} pin(s) found only with the looser fit tried where Inventor inserted the two parts"));
    }
    out.notes.push(format!(
        "joints inferred from the geometry: {count} pivot(s), {} rigid group(s){}",
        out.rigid_groups.len(),
        if unreached == 0 { String::new() } else { format!("; {unreached} part group(s) with no pin or constraint evidence held to the main structure") }
    ));
    out
}

fn overlap(a: &Bbox, b: &Bbox, pad: f64) -> bool {
    a.min.x <= b.max.x + pad && b.min.x <= a.max.x + pad && a.min.y <= b.max.y + pad && b.min.y <= a.max.y + pad && a.min.z <= b.max.z + pad && b.min.z <= a.max.z + pad
}

fn constraint_plan(c: &Constraint, a: RefPlan, b: RefPlan) -> Result<ConstraintPlan, String> {
    let rule = rule_for(&c.kind, c.a.kind, c.b.kind).ok_or_else(|| format!("Inventor `{}` constraints have no SolidCraft rule", c.kind))?;
    // What each rule needs, as SolidCraft checks it, so a constraint it would refuse is evidence.
    let face = |p: &Pick| matches!(p, Pick::Face(_));
    let circle = |p: &Pick| matches!(p, Pick::Circle(_));
    let ok = match rule {
        "mate" | "flush" | "angle" | "parallel" | "perpendicular" => face(&a.pick) && face(&b.pick),
        "insert" => circle(&a.pick) && circle(&b.pick),
        "concentric" => !face(&a.pick) && !face(&b.pick),
        "point_on_axis" => !face(&a.pick) && !face(&b.pick),
        _ => true,
    };
    if !ok {
        return Err(format!("its sides are not the geometry a SolidCraft `{rule}` takes"));
    }
    // Inventor's "aligned" mate faces the same way (SolidCraft's flush); "opposed" is the default.
    let flip = matches!(c.solution.as_deref(), Some("aligned") | Some("inside")) && rule == "mate";
    Ok(ConstraintPlan {
        name: c.name.clone(),
        rule: rule.to_string(),
        a,
        b,
        offset: c.offset.filter(|o| o.abs() > 1e-9),
        angle_deg: c.angle_deg,
        flip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Def, Occ};
    use lcw_geom::v3;

    /// A part's features: round faces on the Z axis through (x, y) and a box of vertices.
    fn part(rounds: &[(f64, f64, f64, bool, f64, f64)], lo: V3, hi: V3, flats: &[(V3, V3)]) -> PartFeatures {
        let mut f = PartFeatures::default();
        for &(x, y, r, hole, z0, z1) in rounds {
            f.rounds.push(Round { axis: Axis::new(v3(x, y, 0.0), V3::Z).unwrap(), radius: r, lo: z0, hi: z1, hole, sweep: std::f64::consts::TAU, mid: None });
        }
        for &(o, n) in flats {
            f.flats.push(Flat { origin: o, normal: n, area: 100.0 });
        }
        f.vertices = vec![lo, hi, v3(lo.x, hi.y, lo.z), v3(hi.x, lo.y, hi.z)];
        f
    }

    fn def(name: &str) -> Def {
        Def { key: name.replace(' ', "_"), name: name.into(), ..Def::default() }
    }

    fn occ(id: &str, def: usize, grounded: bool) -> Occ {
        Occ { id: id.into(), name: id.into(), def, transform: M4::IDENTITY, grounded, visible: true, ..Occ::default() }
    }

    /// An Inventor constraint as the exporter writes one it could not read: (0, 0, 0), no direction.
    fn blind(name: &str, kind: &str, a: &str, b: &str, ek: EntityKind) -> Constraint {
        Constraint {
            id: name.into(),
            name: name.into(),
            kind: kind.into(),
            a: EntityRef { path: vec![a.into()], kind: ek, ..EntityRef::default() },
            b: EntityRef { path: vec![b.into()], kind: ek, ..EntityRef::default() },
            ..Constraint::default()
        }
    }

    /// A frame (grounded) with a 3 mm hole, a lever on it with a 3 mm hole, and a screw through
    /// both: the lever turns on the screw. Sizes keep the frame the biggest.
    fn lever_model(lever_name: &str, constraints: Vec<Constraint>) -> (Model, Vec<Option<PartFeatures>>) {
        let mut m = Model::default();
        m.defs.push(def("bench"));
        m.defs.push(def("frame"));
        m.defs.push(def(lever_name));
        m.defs.push(def("M3 x 20 screw"));
        m.defs[0].assembly = true;
        m.defs[0].occurrences = vec![occ("frame:1", 1, true), occ(&format!("{lever_name}:1"), 2, false), occ("M3 x 20 screw:1", 3, false)];
        m.defs[0].constraints = constraints;
        m.root = 0;
        let features = vec![
            None,
            Some(part(&[(0.0, 0.0, 1.6, true, 0.0, 5.0)], v3(-100.0, -100.0, 0.0), v3(100.0, 100.0, 5.0), &[(v3(0.0, 0.0, 5.0), V3::Z)])),
            Some(part(&[(0.0, 0.0, 1.6, true, 5.0, 8.0)], v3(-5.0, -5.0, 5.0), v3(60.0, 5.0, 8.0), &[(v3(60.0, 0.0, 6.0), V3::X)])),
            Some(part(&[(0.0, 0.0, 1.5, false, -2.0, 12.0)], v3(-1.5, -1.5, -2.0), v3(1.5, 1.5, 12.0), &[])),
        ];
        (m, features)
    }

    fn run(m: &Model, f: &[Option<PartFeatures>]) -> Plan {
        plan(m, &m.instances(), f, &Options::default())
    }

    #[test]
    fn constraints_without_geometry_are_evidence_and_the_geometry_still_finds_the_pivot() {
        let (m, f) = lever_model("lever", vec![blind("Insert:1", "insert", "lever:1", "frame:1", EntityKind::Edge), blind("Flush:1", "flush", "lever:1", "frame:1", EntityKind::Plane)]);
        let p = run(&m, &f);
        assert!(p.constraints.is_empty(), "nothing is built from a constraint with no geometry");
        assert_eq!(p.evidence.len(), 2);
        assert!(p.evidence.iter().all(|e| e.why_not.contains("no exported geometry")), "{:?}", p.evidence);
        assert_eq!(p.joints.len(), 1, "{:?}", p.joints);
        let j = &p.joints[0];
        assert_eq!(j.kind, Kind::Revolute);
        assert_eq!(j.b, vec![1], "the lever moves");
        assert!(j.z.parallel(V3::Z));
        assert_eq!(j.evidence, vec!["Insert:1".to_string()], "Inventor's insert agrees");
        assert!(j.basis().starts_with("geometry and Inventor"), "{}", j.basis());
        assert!(p.evidence.iter().any(|e| e.outcome.contains("joins them")), "{:?}", p.evidence);
        // The screw is held, to the frame; nothing else is in a group.
        assert_eq!(p.rigid_groups.len(), 1);
        assert_eq!(p.rigid_groups[0].members, vec![vec![0], vec![2]]);
        assert!(p.rigid_groups[0].why[1].starts_with("fastener"));
    }

    #[test]
    fn an_angle_constraint_marks_a_part_as_one_that_turns() {
        // No motion word in the name: on its own it would be held.
        let (m, f) = lever_model("selector", vec![]);
        assert!(run(&m, &f).joints.is_empty(), "held without evidence");
        let (m, f) = lever_model("selector", vec![blind("Angle:1", "angle", "selector:1", "frame:1", EntityKind::Plane)]);
        let p = run(&m, &f);
        assert_eq!(p.joints.len(), 1);
        assert!(p.joints[0].basis().contains("Inventor sets its angle (Angle:1)"), "{}", p.joints[0].basis());
    }

    #[test]
    fn a_part_grounded_in_a_sub_assembly_is_not_grounded_in_the_world() {
        let (mut m, mut f) = lever_model("lever", vec![]);
        // The lever becomes a sub-assembly holding the lever, grounded there.
        m.defs.push(def("lever unit"));
        m.defs[4].assembly = true;
        m.defs[4].occurrences = vec![occ("lever:1", 2, true)];
        m.defs[0].occurrences[1] = occ("lever unit:1", 4, false);
        f.push(None);
        let p = run(&m, &f);
        assert_eq!(p.ground, vec![vec![0]], "only the top assembly's ground");
        assert!(p.notes.iter().any(|n| n.contains("grounded inside a sub-assembly")));
        assert_eq!(p.joints.len(), 1, "the lever still turns");
        assert_eq!(p.joints[0].b, vec![1, 0]);
    }

    #[test]
    fn a_constraint_with_complete_geometry_on_the_parts_is_carried_over() {
        let mut c = blind("Mate:1", "mate", "lever:1", "frame:1", EntityKind::Plane);
        c.a.point = v3(10.0, 0.0, 5.0);
        c.a.dir = Some(-V3::Z);
        c.b.point = v3(10.0, 0.0, 5.0);
        c.b.dir = Some(V3::Z);
        let (mut m, mut f) = lever_model("lever", vec![c.clone()]);
        // The lever's underside lies on the frame's top.
        f[2].as_mut().unwrap().flats.push(Flat { origin: v3(0.0, 0.0, 5.0), normal: -V3::Z, area: 50.0 });
        let p = run(&m, &f);
        assert_eq!(p.constraints.len(), 1, "{:?}", p.evidence);
        assert_eq!(p.constraints[0].rule, "mate");
        assert!(matches!(p.constraints[0].a.pick, Pick::Face(_)));
        // A plane that misses every face of the part is evidence, with why.
        c.b.point = v3(10.0, 0.0, 7.0);
        m.defs[0].constraints = vec![c];
        let p = run(&m, &f);
        assert!(p.constraints.is_empty());
        assert!(p.evidence[0].why_not.contains("no planar face"), "{}", p.evidence[0].why_not);
    }

    #[test]
    fn a_looser_fit_is_tried_where_inventor_inserted_two_parts() {
        // The lever's hole is 0.4 mm off the frame's: too far for the strict test.
        let (m, mut f) = lever_model("lever", vec![]);
        f[2] = Some(part(&[(0.4, 0.0, 1.6, true, 5.0, 8.0)], v3(-5.0, -5.0, 5.0), v3(60.0, 5.0, 8.0), &[(v3(60.0, 0.0, 6.0), V3::X)]));
        f[3] = Some(part(&[], v3(-1.5, -1.5, -2.0), v3(1.5, 1.5, 12.0), &[]));
        assert!(run(&m, &f).joints.is_empty());
        let (m, _) = lever_model("lever", vec![blind("Insert:1", "insert", "lever:1", "frame:1", EntityKind::Edge)]);
        let p = run(&m, &f);
        assert_eq!(p.joints.len(), 1, "{:?}", p.notes);
        assert!(p.joints[0].basis().contains("looser fit"), "{}", p.joints[0].basis());
    }

    #[test]
    fn a_part_with_no_pin_is_held_to_what_inventor_constrained_it_to() {
        // A label stuck on the lever: no shared axis, but Inventor mates it to the lever.
        let (mut m, mut f) = lever_model("lever", vec![blind("Mate:7", "mate", "label:1", "lever:1", EntityKind::Plane)]);
        m.defs.push(def("label"));
        m.defs[0].occurrences.push(occ("label:1", 4, false));
        f.push(Some(part(&[], v3(20.0, -2.0, 8.0), v3(30.0, 2.0, 8.5), &[(v3(25.0, 0.0, 8.0), -V3::Z)])));
        let p = run(&m, &f);
        let g = p.rigid_groups.iter().find(|g| g.members.contains(&vec![3])).expect("the label is held");
        assert_eq!(g.members[0], vec![1], "to the lever, so it moves with it");
        assert!(g.why[1].contains("Mate:7"), "{:?}", g.why);
    }
}
