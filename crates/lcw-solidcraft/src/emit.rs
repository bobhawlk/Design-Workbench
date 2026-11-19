//! From the analysis to build steps: one SolidCraft design per part (linked into the assembly,
//! so a part used many times is one file), then the assembly with its sub-assemblies as nested
//! components, its placements, grounding, joints, constraints and patterns.

use std::collections::{BTreeMap, BTreeSet};

use lcw_analyze::joints::{ConstraintPlan, JointPlan, Kind, Pick, RigidGroup};
use lcw_analyze::patterns::Kind as PatKind;
use lcw_analyze::sheet::SheetPlan;
use lcw_analyze::Analysis;
use lcw_geom::{Bbox, M4, V3};
use lcw_json::{obj, Json};

use crate::steps::{var, DesignSteps, Step};

pub struct Options {
    /// Folder the designs are written to (absolute, as SolidCraft will see it).
    pub out_dir: String,
    /// Path separator of the machine SolidCraft runs on.
    pub sep: char,
    /// Make sheet metal parts sheet metal.
    pub sheet_metal: bool,
    /// Set revolute and slider limits from where the parts meet (`joint.find_limits`).
    pub find_limits: bool,
    /// Make patterns' spacing a parameter (linear and grid patterns).
    pub parametric_patterns: bool,
    /// The name of the assembly design.
    pub assembly_name: Option<String>,
    /// Check the mechanism in SolidCraft once the assembly is saved (`verify_joints`).
    pub verify: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options { out_dir: ".".into(), sep: std::path::MAIN_SEPARATOR, sheet_metal: true, find_limits: true, parametric_patterns: true, assembly_name: None, verify: true }
    }
}

/// Everything to build, in order: parts first, then the assembly.
pub struct BuildPlan {
    pub parts: Vec<DesignSteps>,
    pub assembly: DesignSteps,
    /// For each part design: the definition it builds.
    pub part_defs: Vec<usize>,
}

impl BuildPlan {
    pub fn to_json(&self) -> Json {
        let design = |d: &DesignSteps| obj! {"name" => &d.name, "file" => &d.file, "steps" => Json::Arr(d.steps.iter().map(Step::to_json).collect())};
        obj! {
            "schema" => "lcw-build-steps/1",
            "parts" => Json::Arr(self.parts.iter().map(design).collect()),
            "assembly" => design(&self.assembly),
        }
    }
}

fn v3j(p: V3) -> Json {
    let r = |x: f64| (x * 1e6).round() / 1e6 + 0.0;
    Json::from([r(p.x), r(p.y), r(p.z)])
}

fn mm(v: f64) -> String {
    let s = format!("{v:.4}");
    format!("{} mm", s.trim_end_matches('0').trim_end_matches('.'))
}

fn mat16(m: &M4) -> Json {
    // Full precision: rounding the rotation would make it shear by the rounding step, and
    // SolidCraft refuses a placement more than 1e-9 from rigid.
    let r = |x: f64| if x.abs() < 1e-14 { 0.0 } else { x };
    Json::Arr(m.orthonormalised().to_vec16().into_iter().map(|x| Json::from(r(x))).collect())
}

/// A parameter name from free text: letters, digits and underscores, not starting with a digit.
pub fn param_name(s: &str) -> String {
    let mut out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    if out.chars().next().is_none_or(|c| c.is_ascii_digit()) {
        format!("p_{out}")
    } else {
        out
    }
}

/// The steps that build one part's design from its STEP.
pub fn part_steps(a: &Analysis, def: usize, step_path: &str, out_file: &str, opts: &Options) -> DesignSteps {
    let d = &a.model.defs[def];
    let mut steps = vec![Step::tool("new_design", Json::obj(), format!("new design for {}", d.name)).group("new design")];
    steps.push(
        Step::op("insert_step", obj! {"path" => step_path, "name" => d.key.as_str(), "names" => "solid"}, format!("import {}", d.name))
            .bind("part_component", &["/component"])
            .bind("body", &["/created_bodies/0", "/bodies/0/name", "/bodies/0"])
            .group("import"),
    );
    steps.push(Step::op("list_bodies", obj! {"names_only" => true}, "read the body name").soft().bind("body", &["/bodies/0/name", "/bodies/0/definition", "/bodies/0"]).group("import"));
    if opts.sheet_metal {
        if let Some(sp) = a.sheets.get(&def) {
            steps.extend(sheet_steps(sp, &d.name));
        }
    }
    let mut props = Json::obj();
    for (k, v) in &d.properties {
        props.set(k, v.as_str());
    }
    props.set("Source file", a.model.source_file.as_str());
    let mut provenance = obj! {"bridge" => concat!("inv2sc ", env!("CARGO_PKG_VERSION")), "source_app" => a.model.source_app.as_str(), "definition" => d.name.as_str()};
    if let Some(doc) = &d.doc {
        provenance.set("inventor_document", doc.as_str());
    }
    if let Some(m) = a.mirrors.iter().find(|m| m.part == def) {
        provenance.set("mirror_of", a.model.defs[m.of].name.as_str());
        props.set("Mirror of", a.model.defs[m.of].name.as_str());
    }
    // The design's own keys (part number, description) and everything else under the bridge's
    // reverse-DNS key: SolidCraft reserves every other unprefixed key.
    provenance.set("properties", props.clone());
    let mut values = obj! {"io.github.lcw.bridge" => provenance};
    if let Some(pn) = d.part_number.clone().filter(|s| !s.trim().is_empty()) {
        values.set("part_number", pn.as_str());
    }
    if let Some(desc) = d.properties.get("Description").filter(|s| !s.trim().is_empty()) {
        values.set("description", desc.as_str());
    }
    steps.push(Step::op("doc_metadata", obj! {"values" => values}, "part properties").soft().group("properties"));
    // Material and free properties are a component's part properties.
    let mut cm = obj! {"component" => var("part_component"), "properties" => props};
    if let Some(pn) = d.part_number.clone().filter(|s| !s.trim().is_empty()) {
        cm.set("part_number", pn.as_str());
    }
    if let Some(m) = d.material.clone().filter(|s| !s.trim().is_empty()) {
        cm.set("material", m.as_str());
    }
    steps.push(Step::op("component_metadata", cm, "part material and properties").soft().needs("part_component").group("properties"));
    steps.push(Step::tool("save", obj! {"path" => out_file}, format!("save {}", d.name)).group("save"));
    DesignSteps { name: d.name.clone(), file: out_file.to_string(), steps, notes: Vec::new() }
}

fn sheet_steps(sp: &SheetPlan, name: &str) -> Vec<Step> {
    let rule = obj! {"name" => sp.rule.as_str(), "thickness" => mm(sp.thickness), "k_factor" => sp.k_factor, "bend_radius" => mm(sp.bend_radius), "active" => true};
    let bends: Vec<Json> = sp.bends.iter().map(|b| v3j(*b)).collect();
    let mut convert = obj! {"body" => var("body"), "face" => v3j(sp.base_point), "rule" => sp.rule.as_str()};
    if !bends.is_empty() {
        convert.set("bends", Json::Arr(bends));
    }
    vec![
        Step::op("sheet_rule", rule, format!("sheet metal rule {}", sp.rule)).soft().group("sheet metal"),
        Step::op("sheet_convert", convert, format!("make {name} sheet metal")).soft().needs("body").bind("sheet", &["/name", "/id"]).group("sheet metal"),
    ]
}

/// The occurrence vars of every instance (and its full path), filled while placing.
#[derive(Default)]
struct Placed {
    /// instance path → the var naming its occurrence.
    occ: BTreeMap<Vec<usize>, String>,
    /// assembly definition → the var of its component (made once).
    asm_component: BTreeMap<usize, String>,
    /// part definition → the var of its linked component (inserted once).
    part_component: BTreeMap<usize, String>,
    /// assembly definition → the path its children were first created under.
    first_path: BTreeMap<usize, Vec<usize>>,
    /// Instance paths under an assembly placed more than once (they need `path` in joints).
    repeated: BTreeSet<Vec<usize>>,
    /// Paths of sub-assemblies placed as copies of an earlier placement.
    copies: BTreeSet<Vec<usize>>,
    first_part: Option<Vec<usize>>,
}

fn key(path: &[usize]) -> String {
    path.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(".")
}

/// The assembly's steps. `part_files[def]` is the .sc each part design was saved as.
pub fn assembly_steps(a: &Analysis, part_files: &BTreeMap<usize, String>, out_file: &str, opts: &Options) -> DesignSteps {
    let m = &a.model;
    let root = &m.defs[m.root];
    let name = opts.assembly_name.clone().unwrap_or_else(|| root.name.clone());
    let mut steps = vec![Step::tool("new_design", Json::obj(), format!("new design for {name}")).group("new design")];
    let mut placed = Placed::default();
    place_children(a, m.root, &[], None, part_files, &mut placed, &mut steps, opts);
    // Patterns: one parameter per spacing.
    if opts.parametric_patterns {
        for p in &a.patterns {
            steps.extend(pattern_steps(a, p, &placed));
        }
    }
    // Grounding: the planned ground, and the first part SolidCraft grounds by itself released.
    let ground: BTreeSet<Vec<usize>> = a.joints.ground.iter().cloned().collect();
    if let Some(first) = &placed.first_part {
        if !ground.contains(first) {
            if let Some(v) = placed.occ.get(first) {
                steps.push(Step::op("ground", obj! {"occurrence" => var(v), "grounded" => false}, "release the automatic ground").soft().group("grounding"));
            }
        }
    }
    for g in &ground {
        if let Some(v) = placed.occ.get(g) {
            steps.push(Step::op("ground", obj! {"occurrence" => var(v), "grounded" => true}, format!("ground {}", inst_name(a, g))).soft().group("grounding"));
        }
    }
    // Sub-assembly containers stay with the ground (their children move by their own joints).
    let anchor = a.joints.ground.first().cloned();
    let containers: Vec<Vec<usize>> = a.instances.iter().filter(|x| m.defs[x.def].assembly).map(|x| x.path.clone()).collect();
    // Rigid groups.
    let mut groups = a.joints.rigid_groups.clone();
    if let Some(anchor) = &anchor {
        if !containers.is_empty() {
            let mut g = RigidGroup { members: vec![anchor.clone()], why: vec!["anchor".into()] };
            for c in &containers {
                g.members.push(c.clone());
                g.why.push("sub-assembly container (its parts move by their own joints)".into());
            }
            groups.push(g);
        }
    }
    for g in &groups {
        let m = &g.members;
        let addressable = m.iter().all(|p| !placed.repeated.contains(p) && placed.occ.contains_key(p));
        if addressable && m.len() >= 2 {
            let list: Vec<Json> = m.iter().filter_map(|p| placed.occ.get(p)).map(|v| var(v)).collect();
            steps.push(Step::op("rigid_group", obj! {"occurrences" => Json::Arr(list)}, format!("hold {} part(s) to {} ({})", m.len() - 1, inst_name(a, &m[0]), group_why(g))).soft().group("rigid groups"));
        } else {
            for (k, p) in m.iter().enumerate().skip(1) {
                let origin = a.instances.iter().find(|x| &x.path == p).map_or(V3::ZERO, |x| x.world.origin());
                let j = JointPlan {
                    name: format!("{} held", inst_name(a, p)),
                    kind: Kind::Rigid,
                    a: m[0].clone(),
                    b: p.clone(),
                    origin,
                    z: V3::Z,
                    x: Some(V3::X),
                    limits: Vec::new(),
                    source: lcw_analyze::joints::Source::Inferred(g.why.get(k).cloned().unwrap_or_else(|| "rigid group".into())),
                    evidence: Vec::new(),
                };
                steps.push(joint_step(a, &placed, &j, false).group("rigid groups"));
            }
        }
    }
    // Joints and constraints.
    // Joint names are unique: SolidCraft finds a joint by name, and a part placed several times
    // gives several pivots of one name.
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let total: BTreeMap<&str, usize> = a.joints.joints.iter().fold(BTreeMap::new(), |mut m, j| {
        *m.entry(j.name.as_str()).or_insert(0) += 1;
        m
    });
    let boxes = world_boxes(a);
    let mut notes = Vec::new();
    let mut made: Vec<Json> = Vec::new();
    for j in &a.joints.joints {
        // Both parts inside one copy of a repeated sub-assembly: SolidCraft anchors both sides on
        // that copy, so the joint can't be made there (the first placement's joint is).
        let common = j.a.iter().zip(&j.b).take_while(|(x, y)| x == y).count();
        if (1..=common).any(|n| placed.copies.contains(&j.a[..n])) {
            notes.push(format!("{}: not made in the repeated sub-assembly's copy (SolidCraft can't joint two parts inside a copy); the first placement has it", j.name));
            continue;
        }
        let k = seen.entry(j.name.as_str()).or_insert(0);
        *k += 1;
        let name = if total[j.name.as_str()] > 1 { format!("{} {}", j.name, k) } else { j.name.clone() };
        let mut jj = j.clone();
        jj.name = name.clone();
        steps.push(joint_step(a, &placed, &jj, opts.find_limits));
        made.push(obj! {"name" => name.as_str(), "type" => j.kind.name(), "basis" => j.basis()});
        if opts.find_limits && j.limits.is_empty() && matches!(j.kind, Kind::Revolute | Kind::Slider) {
            // Check the moving part only against the parts near it: a contact sweep of the whole
            // assembly is more bodies than SolidCraft sweeps at once.
            let mut args = obj! {"joint" => name.as_str()};
            let occ_of = |path: &[usize]| (1..=path.len()).rev().find_map(|i| placed.occ.get(&path[..i]).map(|v| var(v)));
            if let (Some(mover), Some(bb)) = (occ_of(&j.b), boxes.get(&j.b)) {
                let near: Vec<Json> = boxes
                    .iter()
                    .filter(|(p, b)| **p != j.b && overlaps(bb, b, 2.0))
                    .filter_map(|(p, _)| occ_of(p))
                    .filter(|o| *o != mover)
                    .fold(Vec::new(), |mut v, o| {
                        if !v.contains(&o) {
                            v.push(o);
                        }
                        v
                    });
                if !near.is_empty() && near.len() < 60 {
                    args.set("occurrences", Json::Arr(vec![mover]));
                    args.set("against", Json::Arr(near));
                }
            }
            steps.push(Step::op("find_limits", args, format!("find how far {name} can move")).soft().bind(format!("limits:{name}"), &["/values/0"]).group("limits"));
            steps.push(Step::tool("auto_limits", obj! {"joint" => name.as_str(), "found" => var(&format!("limits:{name}"))}, format!("limit {name}")).soft().needs(format!("limits:{name}")).group("limits"));
        }
    }
    for c in &a.joints.constraints {
        steps.push(constraint_step(&placed, c));
    }
    // What the analysis found, on the design itself.
    let mut props = obj! {"Source file" => m.source_file.as_str(), "Converted by" => concat!("inv2sc ", env!("CARGO_PKG_VERSION"))};
    if !a.symmetry.is_empty() {
        let s: Vec<String> = a.symmetry.iter().map(|p| format!("point {} normal {} ({} pairs)", fmt_v(p.point), fmt_v(p.normal), p.pairs.len())).collect();
        props.set("Symmetry planes", s.join("; "));
    }
    if !a.patterns.is_empty() {
        let s: Vec<String> = a.patterns.iter().map(|p| format!("{}: {}", p.name, describe_pattern(&p.found.kind))).collect();
        props.set("Patterns", s.join("; "));
    }
    steps.push(Step::op("doc_metadata", obj! {"values" => obj!{"io.github.lcw.bridge" => obj!{"properties" => props}}}, "assembly properties").soft().group("properties"));
    steps.push(Step::tool("save", obj! {"path" => out_file}, format!("save {name}")).group("save"));
    if opts.verify {
        // After the save, so the drives it tries can't change the saved design.
        steps.push(Step::tool("verify_joints", obj! {"joints" => Json::Arr(made)}, "check the mechanism: degrees of freedom, conflicts, and drive each joint").soft().group("verification"));
    }
    DesignSteps { name, file: out_file.to_string(), steps, notes }
}

/// What a rigid group is, from its members' reasons.
fn group_why(g: &RigidGroup) -> String {
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for w in g.why.iter().skip(1) {
        let k = w.split(" `").next().unwrap_or(w).split(" (").next().unwrap_or(w).trim().to_string();
        *kinds.entry(k).or_default() += 1;
    }
    let mut s: Vec<String> = kinds.into_iter().map(|(k, n)| if n > 1 { format!("{n} × {k}") } else { k }).collect();
    s.truncate(4);
    let head = g.why.first().filter(|w| w.as_str() != "anchor").map(|w| format!("{w}; ")).unwrap_or_default();
    format!("{head}{}", s.join(", "))
}

fn fmt_v(v: V3) -> String {
    format!("({:.2}, {:.2}, {:.2})", v.x, v.y, v.z)
}

pub fn describe_pattern(k: &PatKind) -> String {
    match k {
        PatKind::Linear { dir, spacing, count } => format!("{count} in a row, {spacing:.3} mm apart along {}", fmt_v(*dir)),
        PatKind::Rectangular { spacing1, count1, spacing2, count2, .. } => format!("{count1} x {count2} grid, {spacing1:.3} x {spacing2:.3} mm"),
        PatKind::Circular { axis, step, count } => format!("{count} round {} every {:.3}°", fmt_v(axis.dir), step.to_degrees()),
    }
}

fn inst_name(a: &Analysis, path: &[usize]) -> String {
    a.model.path_ids(path).last().cloned().unwrap_or_else(|| "?".into())
}

#[allow(clippy::too_many_arguments)]
fn place_children(a: &Analysis, def: usize, prefix: &[usize], parent_var: Option<&str>, part_files: &BTreeMap<usize, String>, placed: &mut Placed, steps: &mut Vec<Step>, opts: &Options) {
    let m = &a.model;
    let repeated_here = prefix.iter().enumerate().any(|(i, _)| placed.repeated.contains(&prefix[..=i].to_vec()));
    for (i, o) in m.defs[def].occurrences.iter().enumerate() {
        if o.suppressed {
            continue;
        }
        let mut path = prefix.to_vec();
        path.push(i);
        let vname = format!("occ:{}", key(&path));
        let child = &m.defs[o.def];
        let parent = parent_var.map(var).unwrap_or(Json::Null);
        if child.assembly {
            match placed.asm_component.get(&o.def).cloned() {
                None => {
                    let cvar = format!("comp:{}", child.key);
                    steps.push(
                        Step::op("create_component", obj! {"name" => child.key.as_str(), "parent" => parent.clone(), "activate" => false, "grounded" => false}, format!("sub-assembly {}", child.name))
                            .bind(&cvar, &["/component"])
                            .bind(&vname, &["/occurrence"])
                            .group("components"),
                    );
                    placed.asm_component.insert(o.def, cvar.clone());
                    placed.first_path.insert(o.def, path.clone());
                    placed.occ.insert(path.clone(), vname.clone());
                    steps.push(place_step(&vname, &o.transform, &o.id));
                    place_children(a, o.def, &path, Some(&cvar), part_files, placed, steps, opts);
                }
                Some(cvar) => {
                    steps.push(Step::op("copy_occurrence", obj! {"component" => var(&cvar), "parent" => parent.clone()}, format!("another {}", child.name)).bind(&vname, &["/occurrence"]).group("components"));
                    placed.occ.insert(path.clone(), vname.clone());
                    placed.copies.insert(path.clone());
                    steps.push(place_step(&vname, &o.transform, &o.id));
                    // Its contents are the first copy's; map their paths onto the first copy's vars.
                    let first = placed.first_path.get(&o.def).cloned().unwrap_or_default();
                    let under: Vec<(Vec<usize>, String)> = placed.occ.iter().filter(|(p, _)| p.len() > first.len() && p.starts_with(&first)).map(|(p, v)| (p.clone(), v.clone())).collect();
                    for (p, v) in under {
                        let mut q = path.clone();
                        q.extend_from_slice(&p[first.len()..]);
                        placed.repeated.insert(q.clone());
                        placed.occ.insert(q, v);
                    }
                    placed.repeated.insert(path.clone());
                    for p in placed.occ.keys().filter(|p| p.starts_with(&first) && p.len() > first.len()).cloned().collect::<Vec<_>>() {
                        placed.repeated.insert(p);
                    }
                }
            }
        } else {
            let Some(file) = part_files.get(&o.def) else { continue };
            match placed.part_component.get(&o.def).cloned() {
                None => {
                    let cvar = format!("part:{}", child.key);
                    steps.push(
                        Step::op("insert_design", obj! {"path" => file.as_str(), "name" => child.key.as_str(), "parent" => parent.clone()}, format!("place {}", child.name))
                            .bind(&cvar, &["/component"])
                            .bind(&vname, &["/occurrence"])
                            .group("components"),
                    );
                    placed.part_component.insert(o.def, cvar);
                    if placed.first_part.is_none() && prefix.is_empty() {
                        placed.first_part = Some(path.clone());
                    }
                }
                Some(cvar) => {
                    steps.push(Step::op("copy_occurrence", obj! {"component" => var(&cvar), "parent" => parent.clone()}, format!("another {}", child.name)).bind(&vname, &["/occurrence"]).group("components"));
                }
            }
            placed.occ.insert(path.clone(), vname.clone());
            steps.push(place_step(&vname, &o.transform, &o.id));
        }
        if repeated_here {
            placed.repeated.insert(path);
        }
    }
}

fn place_step(vname: &str, t: &M4, id: &str) -> Step {
    Step::op("place", obj! {"occurrence" => var(vname), "to_matrix" => mat16(t), "space" => "local", "move_grounded" => true}, format!("place {id}")).group("placement")
}

/// A joint origin on one side.
fn origin(a: &Analysis, placed: &Placed, path: &[usize], point: V3, z: V3, x: V3) -> Json {
    let mut o = obj! {"occurrence" => placed.occ.get(path).map(|v| var(v)).unwrap_or(Json::Null), "point" => v3j(point), "z" => v3j(z), "x" => v3j(x)};
    if placed.repeated.contains(path) {
        let mut full = Vec::new();
        for i in 1..=path.len() {
            if let Some(v) = placed.occ.get(&path[..i]) {
                full.push(var(v));
            }
        }
        o.set("path", Json::Arr(full));
    }
    let _ = a;
    o
}

fn joint_step(a: &Analysis, placed: &Placed, j: &JointPlan, _limits_later: bool) -> Step {
    let z = j.z.unit().unwrap_or(V3::Z);
    let x = j.x.and_then(|x| (x - z * x.dot(z)).unit()).unwrap_or_else(|| z.any_perp());
    let mut p = obj! {
        "type" => j.kind.name(),
        "a" => origin(a, placed, &j.a, j.origin, z, x),
        "b" => origin(a, placed, &j.b, j.origin, z, x),
        "flip" => true,
        "name" => j.name.as_str(),
        "quiet" => true,
    };
    if !j.limits.is_empty() {
        p.set("limits", Json::Arr(j.limits.iter().map(|l| Json::from(*l)).collect()));
    }
    Step::op("joint", p, format!("{} {} ({})", j.kind.name(), j.name, j.basis())).soft().group("joints")
}

fn pick_origin(placed: &Placed, path: &[usize], pick: &Pick) -> Json {
    let occ = placed.occ.get(path).map(|v| var(v)).unwrap_or(Json::Null);
    let mut o = match pick {
        Pick::Face(p) => obj! {"occurrence" => occ, "face" => v3j(*p)},
        Pick::Circle(p) => obj! {"occurrence" => occ, "circle" => v3j(*p)},
        Pick::Frame { point, z } => obj! {"occurrence" => occ, "point" => v3j(*point), "z" => v3j(*z)},
    };
    if placed.repeated.contains(path) {
        let full: Vec<Json> = (1..=path.len()).filter_map(|i| placed.occ.get(&path[..i]).map(|v| var(v))).collect();
        o.set("path", Json::Arr(full));
    }
    o
}

fn constraint_step(placed: &Placed, c: &ConstraintPlan) -> Step {
    let mut p = obj! {
        "type" => c.rule.as_str(),
        "a" => pick_origin(placed, &c.a.path, &c.a.pick),
        "b" => pick_origin(placed, &c.b.path, &c.b.pick),
        "name" => c.name.as_str(),
        "quiet" => true,
    };
    if let Some(o) = c.offset {
        p.set("offset", o);
    }
    if let Some(g) = c.angle_deg {
        p.set("angle", format!("{g} deg"));
    }
    if c.flip {
        p.set("flip", true);
    }
    // No fallback to a joint: a joint made from a constraint's rule, at a guessed frame, frees or
    // locks parts the constraint does not. A constraint SolidCraft refuses is reported.
    let step = Step::op("constraint", p, format!("{} {} (Inventor constraint)", c.rule, c.name)).soft().group("constraints");
    step
}

fn pattern_steps(a: &Analysis, p: &lcw_analyze::PatternPlan, placed: &Placed) -> Vec<Step> {
    // Members are occurrence indices of the pattern's assembly; find their instance paths.
    let prefixes: Vec<Vec<usize>> = if p.assembly == a.model.root {
        vec![Vec::new()]
    } else {
        a.instances.iter().filter(|x| x.def == p.assembly).map(|x| x.path.clone()).take(1).collect()
    };
    let Some(prefix) = prefixes.into_iter().next() else { return Vec::new() };
    let defs = &a.model.defs[p.assembly];
    let mut steps = Vec::new();
    let (params, dirs): (Vec<(String, f64)>, Vec<V3>) = match &p.found.kind {
        PatKind::Linear { dir, spacing, .. } => (vec![(param_name(&format!("{} spacing", p.name)), *spacing)], vec![*dir]),
        PatKind::Rectangular { dir1, spacing1, dir2, spacing2, .. } => {
            (vec![(param_name(&format!("{} spacing 1", p.name)), *spacing1), (param_name(&format!("{} spacing 2", p.name)), *spacing2)], vec![*dir1, *dir2])
        }
        PatKind::Circular { .. } => return Vec::new(),
    };
    for (n, v) in &params {
        steps.push(Step::op("parameter", obj! {"name" => n.as_str(), "expression" => mm(*v), "comment" => format!("{}: {}", p.name, describe_pattern(&p.found.kind))}, format!("parameter {n}")).soft().bind(format!("param:{n}"), &["/name", "/changed"]).group("patterns"));
    }
    let seed = p.found.members[0];
    let seed_t = defs.occurrences[seed].transform;
    for (k, &mi) in p.found.members.iter().enumerate().skip(1) {
        let idx = &p.found.index[k];
        let mut path = prefix.clone();
        path.push(mi);
        let Some(v) = placed.occ.get(&path) else { continue };
        let expr = |axis: usize| -> Json {
            let mut terms = Vec::new();
            for (d, (dir, (n, _))) in dirs.iter().zip(&params).enumerate() {
                let comp = [dir.x, dir.y, dir.z][axis];
                let count = idx.get(d).copied().unwrap_or(0) as f64;
                if comp.abs() > 1e-9 && count != 0.0 {
                    terms.push(format!("{} * {}", fmt_num(comp * count), n));
                }
            }
            if terms.is_empty() {
                Json::from(0)
            } else {
                Json::Str(terms.join(" + "))
            }
        };
        let final_t = defs.occurrences[mi].transform;
        steps.push(Step::op("place", obj! {"occurrence" => var(v), "to_matrix" => mat16(&seed_t), "space" => "local", "move_grounded" => true}, format!("pattern seed position for {}", defs.occurrences[mi].id)).soft().group("patterns"));
        steps.push(
            Step::op("place", obj! {"occurrence" => var(v), "translate" => Json::Arr(vec![expr(0), expr(1), expr(2)]), "space" => "local", "move_grounded" => true}, format!("pattern member {} follows the spacing", defs.occurrences[mi].id))
                .soft()
                .group("patterns")
                .or_else("place", obj! {"occurrence" => var(v), "to_matrix" => mat16(&final_t), "space" => "local", "move_grounded" => true}),
        );
    }
    steps
}

fn fmt_num(v: f64) -> String {
    let s = format!("{v:.9}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Each placed part's box in the assembly's coordinates, by instance path.
fn world_boxes(a: &Analysis) -> BTreeMap<Vec<usize>, Bbox> {
    let mut out = BTreeMap::new();
    for inst in &a.instances {
        let d = &a.model.defs[inst.def];
        if d.assembly || d.solids.is_empty() {
            continue;
        }
        let pts = d.solids.iter().flat_map(|s| s.faces.iter().flat_map(|f| f.vertices.iter())).map(|p| inst.world.point(*p));
        let b = Bbox::of(pts);
        if !b.is_empty() {
            out.insert(inst.path.clone(), b);
        }
    }
    out
}

fn overlaps(a: &Bbox, b: &Bbox, pad: f64) -> bool {
    a.min.x - pad <= b.max.x && b.min.x - pad <= a.max.x && a.min.y - pad <= b.max.y && b.min.y - pad <= a.max.y && a.min.z - pad <= b.max.z && b.min.z - pad <= a.max.z
}
