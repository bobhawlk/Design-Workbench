//! How the parts are held together: Inventor's constraints and joints carried over, and for parts
//! that have none, joints read off the geometry.
//!
//! Inference, for parts Inventor left unconstrained:
//! 1. Two parts are *pinned* on an axis where a hole of one lines up with a hole or pin of the
//!    other (coaxial, radii within a clearance fit, overlapping or touching along the axis).
//! 2. Fasteners (screws, nuts, washers…) go rigidly with the part they pass through, and parts
//!    that are round about a single axis (spacers, bushes, shafts) with their biggest neighbour.
//! 3. Parts pinned to each other on two or more different axes are rigid together; repeat on the
//!    rigid groups until nothing changes.
//! 4. The group holding the grounded part (or the biggest) is the ground. Every other group pinned
//!    to another on one axis turns there (a revolute joint). Groups left with no joint stay where
//!    they are, rigid to the ground, and the report lists them.

use std::collections::{BTreeMap, BTreeSet};

use lcw_bundle::{Constraint, EntityKind, EntityRef};
use lcw_geom::{Axis, Bbox, M4, V3};

use crate::features::{PartFeatures, Round};
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

#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The grounded instances.
    pub ground: Vec<Vec<usize>>,
    pub joints: Vec<JointPlan>,
    pub constraints: Vec<ConstraintPlan>,
    /// Groups of instances held rigidly together (first is the anchor), from inference.
    pub rigid_groups: Vec<Vec<Vec<usize>>>,
    pub notes: Vec<String>,
}

pub struct Options {
    /// Infer joints for parts Inventor left unconstrained.
    pub infer: bool,
    /// Coaxial tolerance (mm).
    pub axis_tol: f64,
    /// Largest radius difference for two round faces to count as a fit (mm).
    pub fit: f64,
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

fn pick_of(e: &EntityRef, to_world: &M4) -> Pick {
    let p = to_world.point(e.point);
    match e.kind {
        EntityKind::Plane => Pick::Face(p),
        EntityKind::Cylinder | EntityKind::Cone | EntityKind::Edge if e.rim_point.is_some() => Pick::Circle(to_world.point(e.rim_point.unwrap_or(e.point))),
        _ => Pick::Frame { point: p, z: e.dir.map(|d| to_world.dir(d)).unwrap_or(V3::Z) },
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

pub fn plan(model: &Model, instances: &[Inst], features: &[Option<PartFeatures>], opts: &Options) -> Plan {
    let mut out = Plan::default();
    let index: BTreeMap<Vec<usize>, usize> = instances.iter().enumerate().map(|(i, x)| (x.path.clone(), i)).collect();
    // Assembly instances (and the root) by path, for constraints in sub-assemblies.
    let mut asm_places: Vec<(usize, Vec<usize>, M4)> = vec![(model.root, Vec::new(), M4::IDENTITY)];
    for x in instances {
        if model.defs[x.def].assembly {
            asm_places.push((x.def, x.path.clone(), x.world));
        }
    }
    let mut constrained: BTreeSet<Vec<usize>> = BTreeSet::new();
    for (def, base, world) in &asm_places {
        let d = &model.defs[*def];
        for c in d.constraints.iter().filter(|c| !c.suppressed) {
            match constraint_plan(model, *def, base, world, c) {
                Ok(p) => {
                    constrained.insert(p.a.path.clone());
                    constrained.insert(p.b.path.clone());
                    out.constraints.push(p);
                }
                Err(why) => out.notes.push(format!("{} › {}: {why}", d.name, c.name)),
            }
        }
        for j in d.joints.iter().filter(|j| !j.suppressed) {
            let (Some(ka), Some(kb)) = (model.resolve(*def, &j.a.path), model.resolve(*def, &j.b.path)) else {
                out.notes.push(format!("{} › {}: a side names an occurrence that is not in the assembly", d.name, j.name));
                continue;
            };
            let Some(kind) = Kind::from_inventor(&j.kind) else {
                out.notes.push(format!("{} › {}: Inventor joint type `{}` has no SolidCraft equivalent; left out", d.name, j.name, j.kind));
                continue;
            };
            let a: Vec<usize> = base.iter().copied().chain(ka).collect();
            let b: Vec<usize> = base.iter().copied().chain(kb).collect();
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
            });
        }
        for (i, o) in d.occurrences.iter().enumerate() {
            if o.grounded && !o.suppressed {
                let mut p = base.clone();
                p.push(i);
                out.ground.push(p.clone());
                constrained.insert(p);
            }
        }
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
            Some(Body {
                inst: i,
                name: name.clone(),
                rounds: f.rounds.iter().map(|r| r.transformed(&x.world)).collect(),
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
    // 1. Pins between pairs: the distinct shared axes.
    let mut shared: BTreeMap<(usize, usize), Vec<Axis>> = BTreeMap::new();
    for a in 0..n {
        for b in a + 1..n {
            if !overlap(&bodies[a].bbox, &bodies[b].bbox, 1.0) {
                continue;
            }
            let mut axes: Vec<Axis> = Vec::new();
            for ra in &bodies[a].rounds {
                for rb in &bodies[b].rounds {
                    if !(ra.hole || rb.hole) || (ra.radius - rb.radius).abs() > opts.fit.max(0.1 * ra.radius.min(rb.radius)) {
                        continue;
                    }
                    if !ra.axis.coincident(&rb.axis, opts.axis_tol) {
                        continue;
                    }
                    // Along the axis, in a's parameter.
                    let (alo, ahi) = (ra.lo, ra.hi);
                    let shift = (rb.axis.origin - ra.axis.origin).dot(ra.axis.dir);
                    let s = if rb.axis.dir.dot(ra.axis.dir) > 0.0 { 1.0 } else { -1.0 };
                    let (b1, b2) = (shift + s * rb.lo, shift + s * rb.hi);
                    let (blo, bhi) = (b1.min(b2), b1.max(b2));
                    if blo > ahi + 1.0 || alo > bhi + 1.0 {
                        continue;
                    }
                    if !axes.iter().any(|x| x.coincident(&ra.axis, opts.axis_tol * 2.0)) {
                        axes.push(ra.axis.normalised());
                    }
                }
            }
            if !axes.is_empty() {
                shared.insert((a, b), axes);
            }
        }
    }
    // Union-find over bodies.
    let mut parent: Vec<usize> = (0..n).collect();
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
    let mut reason: BTreeMap<usize, String> = BTreeMap::new();
    // 2. Fasteners and round parts go with a neighbour.
    let neighbours = |i: usize| -> Vec<(usize, usize)> {
        shared.iter().filter_map(|((a, b), ax)| if *a == i { Some((*b, ax.len())) } else if *b == i { Some((*a, ax.len())) } else { None }).collect()
    };
    for i in 0..n {
        if bodies[i].fastener {
            let host = neighbours(i).into_iter().filter(|(j, _)| !bodies[*j].fastener).max_by(|a, b| bodies[a.0].size.total_cmp(&bodies[b.0].size));
            if let Some((j, _)) = host {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[ri] = rj;
                reason.insert(i, "fastener".into());
            }
        }
    }
    // 3. Two or more axes between groups make them rigid; iterate.
    loop {
        let mut by_pair: BTreeMap<(usize, usize), Vec<Axis>> = BTreeMap::new();
        for ((a, b), axes) in &shared {
            let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
            if ra == rb {
                continue;
            }
            let key = (ra.min(rb), ra.max(rb));
            let list = by_pair.entry(key).or_default();
            for ax in axes {
                if !list.iter().any(|x| x.coincident(ax, opts.axis_tol * 2.0)) {
                    list.push(*ax);
                }
            }
        }
        let merge: Vec<(usize, usize)> = by_pair.iter().filter(|(_, ax)| ax.len() >= 2).map(|(k, _)| *k).collect();
        if merge.is_empty() {
            break;
        }
        for (a, b) in merge {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                parent[ra] = rb;
            }
        }
    }
    // Ground group: the grounded instance's, else the biggest.
    let grounded_body = bodies.iter().position(|b| out.ground.iter().any(|g| g == &instances[b.inst].path));
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
        if !bodies[i].axisym || bodies[i].fastener || find(&mut parent, i) != i || parent.iter().filter(|p| **p == i).count() > 1 {
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
            reason.insert(i, "round part".into());
        }
    }
    // Re-run the two-axis merge after the round parts joined their hosts.
    loop {
        let mut by_pair: BTreeMap<(usize, usize), Vec<Axis>> = BTreeMap::new();
        for ((a, b), axes) in &shared {
            let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
            if ra == rb {
                continue;
            }
            let list = by_pair.entry((ra.min(rb), ra.max(rb))).or_default();
            for ax in axes {
                if !list.iter().any(|x| x.coincident(ax, opts.axis_tol * 2.0)) {
                    list.push(*ax);
                }
            }
        }
        let merge: Vec<(usize, usize)> = by_pair.iter().filter(|(_, ax)| ax.len() >= 2).map(|(k, _)| *k).collect();
        if merge.is_empty() {
            break;
        }
        for (a, b) in merge {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                parent[ra] = rb;
            }
        }
    }
    let seed = match grounded_body {
        Some(g) => g,
        None => {
            let snapshot = parent.clone();
            (0..n).find(|i| find(&mut snapshot.clone(), *i) == ground_root).unwrap_or(0)
        }
    };
    let ground_root = find(&mut parent, seed);
    // A group pinned on one axis only turns when it hangs off the structure and its name says it
    // moves; anything else riding on one pin is held to what it rides on. Repeat until stable.
    let says = |members: &[usize], words: &[String]| members.iter().any(|m| words.iter().any(|w| bodies[*m].name.contains(w.as_str())));
    loop {
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in 0..n {
            let r = find(&mut parent, i);
            members.entry(r).or_default().push(i);
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
            let moves = says(&ms, &opts.motion_words) && !says(&ms[..1.min(ms.len())], &opts.fixed_words);
            if on_ground && moves {
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
            for m in &ms {
                reason.entry(*m).or_insert_with(|| "held: one pin, no sign of motion".into());
            }
            let rg = find(&mut parent, *g);
            let rh = find(&mut parent, host);
            if rg != rh {
                parent[rg] = rh;
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    // Groups and their members, biggest member first.
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    for members in groups.values_mut() {
        members.sort_by(|a, b| bodies[*b].size.total_cmp(&bodies[*a].size));
    }
    let free = |path: &Vec<usize>| !constrained.contains(path);
    // 4. Revolute joints between groups pinned on one axis.
    let mut pins: BTreeMap<(usize, usize), (Axis, usize, usize)> = BTreeMap::new();
    for ((a, b), axes) in &shared {
        let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
        if ra == rb || axes.len() != 1 {
            continue;
        }
        let key = (ra.min(rb), ra.max(rb));
        let score = bodies[*a].size + bodies[*b].size;
        match pins.get(&key) {
            Some((_, pa, pb)) if bodies[*pa].size + bodies[*pb].size >= score => {}
            _ => {
                pins.insert(key, (axes[0], *a, *b));
            }
        }
    }
    // Groups reachable from the ground through pins move; the rest stay rigid.
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
    let anchor_of = |g: usize, groups: &BTreeMap<usize, Vec<usize>>| groups.get(&g).and_then(|m| m.first().copied()).unwrap_or(g);
    let ground_anchor = anchor_of(ground_root, &groups);
    if grounded_body.is_none() && free(&instances[bodies[ground_anchor].inst].path) {
        out.ground.push(instances[bodies[ground_anchor].inst].path.clone());
        out.notes.push(format!("grounded `{}`, the biggest part of the main structure", model.defs[instances[bodies[ground_anchor].inst].def].name));
    }
    let mut count = 0usize;
    for ((ga, gb), (axis, pa, pb)) in &pins {
        let (pa_path, pb_path) = (instances[bodies[*pa].inst].path.clone(), instances[bodies[*pb].inst].path.clone());
        if !free(&pa_path) && !free(&pb_path) {
            continue;
        }
        // a is on the ground's side, b moves.
        let _ = (ga, gb);
        let (a, b) = if find(&mut parent, *pb) == ground_root { (pb, pa) } else { (pa, pb) };
        let ra = bodies[*b].rounds.iter().find(|r| r.axis.coincident(axis, opts.axis_tol * 2.0));
        let origin = ra.map_or(axis.origin, |r| r.axis.origin + r.axis.dir * r.axial_mid());
        count += 1;
        out.joints.push(JointPlan {
            name: format!("{} pivot", model.defs[instances[bodies[*b].inst].def].name),
            kind: Kind::Revolute,
            a: instances[bodies[*a].inst].path.clone(),
            b: instances[bodies[*b].inst].path.clone(),
            origin,
            z: axis.dir,
            x: None,
            limits: Vec::new(),
            source: Source::Inferred(format!("pinned on one axis to `{}`", model.defs[instances[bodies[*a].inst].def].name)),
        });
    }
    // Rigid groups (anchor first), only for free members; unreached groups join the ground.
    let mut unreached = Vec::new();
    for (g, members) in &groups {
        let mut list: Vec<Vec<usize>> = members.iter().map(|i| instances[bodies[*i].inst].path.clone()).collect();
        if !reach.contains(g) && *g != ground_root {
            let anchor = list[0].clone();
            if free(&anchor) {
                unreached.push(model.defs[instances[bodies[members[0]].inst].def].name.clone());
                // Hold it to the ground's anchor.
                list.insert(0, instances[bodies[ground_anchor].inst].path.clone());
            }
        }
        let anchor = list[0].clone();
        let rest: Vec<Vec<usize>> = list.into_iter().skip(1).filter(|p| free(p)).collect();
        if !rest.is_empty() {
            let mut group = vec![anchor];
            group.extend(rest);
            out.rigid_groups.push(group);
        }
    }
    let _ = index;
    out.notes.push(format!(
        "joints inferred from the geometry: {count} pivot(s), {} rigid group(s){}",
        out.rigid_groups.len(),
        if unreached.is_empty() { String::new() } else { format!("; {} part group(s) with no joint held to the ground", unreached.len()) }
    ));
    out
}

fn overlap(a: &Bbox, b: &Bbox, pad: f64) -> bool {
    a.min.x <= b.max.x + pad && b.min.x <= a.max.x + pad && a.min.y <= b.max.y + pad && b.min.y <= a.max.y + pad && a.min.z <= b.max.z + pad && b.min.z <= a.max.z + pad
}

fn constraint_plan(model: &Model, asm: usize, base: &[usize], world: &M4, c: &Constraint) -> Result<ConstraintPlan, String> {
    let rule = rule_for(&c.kind, c.a.kind, c.b.kind).ok_or_else(|| format!("Inventor `{}` constraints have no SolidCraft rule; left out", c.kind))?;
    let ka = model.resolve(asm, &c.a.path).ok_or("its first part is not in the assembly")?;
    let kb = model.resolve(asm, &c.b.path).ok_or("its second part is not in the assembly")?;
    let a = RefPlan { path: base.iter().copied().chain(ka).collect(), pick: pick_of(&c.a, world) };
    let b = RefPlan { path: base.iter().copied().chain(kb).collect(), pick: pick_of(&c.b, world) };
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
