//! The checks. SolidCraft is asked what it holds; the bundle says what Inventor had; the two
//! are compared here. Nothing of the converter's own planning is used to decide what is right.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lcw_bundle::{Bundle, DocKind, Document};
use lcw_geom::{Bbox, V3};
use lcw_json::{obj, Json};

use crate::assign::{assign, FORBIDDEN};
use crate::expect::{expand, ExpInst, Expected};
use crate::judge::{frame_delta, judge, rel_diff, transformed_box, Judged, Tol};
use crate::obs::{measure_design, name_root, parse_links, step_vertices, Geo, Inst, Measured, Tree};
use crate::session::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// Information, or a check that could not be made (the summary says which).
    Info,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Info => "INFO",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub id: u8,
    pub title: String,
    pub status: Status,
    pub summary: String,
    pub details: Vec<String>,
    pub data: Json,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub bundle: String,
    pub built: String,
    pub assembly: String,
    pub solidcraft: String,
    pub checks: Vec<Check>,
    /// Things that could not be checked, and why.
    pub limits: Vec<String>,
    pub seconds: f64,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }
    pub fn check(&self, id: u8) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }
    /// One line for the terminal.
    pub fn summary_line(&self) -> String {
        let parts: Vec<String> = self.checks.iter().map(|c| format!("{} {}", c.id, c.status.name())).collect();
        format!("verify: {} ({})", if self.failed() { "FAIL" } else { "PASS" }, parts.join(", "))
    }
}

pub struct Inputs<'a> {
    pub bundle: &'a Bundle,
    /// The folder the built designs are in.
    pub built: &'a Path,
    /// The assembly design to check.
    pub assembly: &'a Path,
    pub tol: Tol,
    /// Progress lines.
    pub log: &'a dyn Fn(&str),
}

/// A bundle part and what SolidCraft says about it, from its STEP and from its built design.
#[derive(Debug, Clone, Default)]
struct PartData {
    reference: Measured,
    verts: Vec<V3>,
    verts_note: Option<String>,
    /// The STEP holds no solid (a reference sketch or surfaces): not buildable, not expected.
    no_solid: bool,
    built_file: Option<String>,
    built: Option<Measured>,
}

struct Obs {
    inst: Inst,
    geo: Option<Geo>,
    error: Option<String>,
}

fn fmt(x: f64) -> String {
    if x.abs() >= 100.0 {
        format!("{x:.1}")
    } else {
        format!("{x:.3}")
    }
}

fn pathname(names: &[String]) -> String {
    names.join(" > ")
}

fn box_json(b: &Bbox) -> Json {
    obj! {"min" => b.min.to_array(), "max" => b.max.to_array()}
}

fn compatible(e: &ExpInst, o: &Inst, rg: Option<&Geo>, og: Option<&Geo>, vol_tol: f64) -> bool {
    if name_root(o.name()) == e.doc_name || o.comp_name == e.doc_name || name_root(o.name()) == name_root(e.name()) {
        return true;
    }
    match (rg.and_then(|g| g.volume), og.and_then(|g| g.volume)) {
        (Some(a), Some(b)) => rel_diff(a, b) <= vol_tol,
        _ => false,
    }
}

pub fn run(s: &mut Session, inp: &Inputs) -> Result<Report, String> {
    let t0 = std::time::Instant::now();
    let bundle = inp.bundle;
    let tol = &inp.tol;
    let log = inp.log;
    let expected: Expected = expand(bundle)?;
    let mut report = Report { bundle: bundle.folder.as_ref().map(|f| f.display().to_string()).unwrap_or_default(), built: inp.built.display().to_string(), assembly: inp.assembly.display().to_string(), ..Report::default() };
    if let Ok(v) = s.t.call("version", Json::obj()) {
        report.solidcraft = format!("{} ({})", v.str_of("version").unwrap_or("?"), v.str_of("mode").unwrap_or("?"));
    }

    // 1. The built assembly: its tree, its links, each part instance measured in the world.
    log("opening the built assembly");
    s.open(&inp.assembly.display().to_string()).map_err(|e| format!("could not open the assembly {}: {e}", inp.assembly.display()))?;
    let list = s.op("list_components", obj! {"compact" => true, "bodies" => false})?;
    let tree = Tree::parse(s.p, &list);
    let insts = tree.instances();
    let links = s.op("component_links", Json::obj()).map(|j| parse_links(s.p, &j)).unwrap_or_default();
    let mut observed: Vec<Obs> = Vec::new();
    let leaves = insts.iter().filter(|i| i.leaf).count();
    for (n, inst) in insts.iter().enumerate() {
        if !inst.leaf {
            observed.push(Obs { inst: inst.clone(), geo: None, error: None });
            continue;
        }
        if n % 20 == 0 {
            log(&format!("measuring the built placements ({n}/{})", insts.len()));
        }
        let path: Vec<Json> = inst.path.iter().map(|x| Json::from(*x as f64)).collect();
        let r = s.op("occurrence_props", obj! {"occurrence" => *inst.path.last().unwrap_or(&0) as f64, "path" => Json::Arr(path), "descendants" => true});
        match r {
            Ok(j) => observed.push(Obs { inst: inst.clone(), geo: Some(Geo::from_props(s.p, &j)), error: None }),
            Err(e) => observed.push(Obs { inst: inst.clone(), geo: None, error: Some(e) }),
        }
    }
    let _ = leaves;
    // The world's bodies, as SolidCraft measures them (also names the closed ones for the clash check).
    log("measuring the world bodies");
    let world = s.op("verify_measure", obj! {"summary" => true});
    // Interference, on the closed bodies only: a surface has no volume to share.
    let mut closed_names: Vec<String> = Vec::new();
    let mut body_volume: BTreeMap<String, f64> = BTreeMap::new();
    let mut world_box: Option<Bbox> = None;
    if let Ok(w) = &world {
        let k = |key: &str| s.p.vname("measure", key);
        for b in w.arr_of(&k("bodies")) {
            let name = b.str_of(&k("name")).unwrap_or_default().to_string();
            if let Some(bb) = b.get(&k("bbox")).and_then(|x| lcw_json::to_array::<3>(x.get("min")?).zip(lcw_json::to_array::<3>(x.get("max")?))) {
                let bx = Bbox { min: V3::from_array(bb.0), max: V3::from_array(bb.1) };
                match &mut world_box {
                    Some(wb) => {
                        wb.add(bx.min);
                        wb.add(bx.max);
                    }
                    None => world_box = Some(bx),
                }
            }
            if b.bool_of(&k("closed")).unwrap_or(false) {
                closed_names.push(name.clone());
                if let Some(v) = b.f64_of(&k("volume")) {
                    body_volume.insert(name, v);
                }
            }
        }
    }
    log("checking for interference");
    let clash = if s.p.available("interference") && !closed_names.is_empty() {
        let names: Vec<Json> = closed_names.iter().map(|n| Json::from(n.as_str())).collect();
        let budget = std::time::Duration::from_secs_f64(tol.interference_budget_s + 300.0);
        Some(s.op_within("interference", obj! {"bodies" => Json::Arr(names), "budget_s" => tol.interference_budget_s}, Some(budget)))
    } else {
        None
    };

    // 2. What SolidCraft makes of each bundle part's STEP, and its vertices read from the file.
    let part_docs: Vec<&Document> = bundle.documents.iter().filter(|d| d.kind == DocKind::Part && d.step.is_some()).collect();
    let mut parts: BTreeMap<String, PartData> = BTreeMap::new();
    for (n, d) in part_docs.iter().enumerate() {
        log(&format!("reference import {}/{}: {}", n + 1, part_docs.len(), d.name));
        let step = bundle.path(d.step.as_deref().unwrap_or(""));
        let mut pd = PartData { reference: measure_design(s, &step.display().to_string()), ..PartData::default() };
        pd.no_solid = pd.reference.error.as_deref().is_some_and(|e| e.contains("no bodies") || e.contains("no solids"));
        match step_vertices(&step) {
            Ok(v) => pd.verts = v,
            Err(e) => pd.verts_note = Some(e),
        }
        parts.insert(d.id.clone(), pd);
    }

    // 3. Match the bundle's placements to the built ones.
    let exp_leaves: Vec<&ExpInst> = expected.insts.iter().filter(|e| !e.is_assembly && e.has_step && parts.get(&e.doc).is_some_and(|p| !p.no_solid)).collect();
    let obs_leaves: Vec<&Obs> = observed.iter().filter(|o| o.inst.leaf).collect();
    let ref_geo = |doc: &str| parts.get(doc).and_then(|p| p.reference.geo.as_ref());
    let mut cost = vec![vec![FORBIDDEN; obs_leaves.len()]; exp_leaves.len()];
    for (i, e) in exp_leaves.iter().enumerate() {
        for (k, o) in obs_leaves.iter().enumerate() {
            if !compatible(e, &o.inst, ref_geo(&e.doc), o.geo.as_ref(), 0.01) {
                continue;
            }
            cost[i][k] = match (ref_geo(&e.doc), o.geo.as_ref()) {
                (Some(rg), Some(og)) => {
                    let j = judge(rg, &parts[&e.doc].verts, &e.world, og, &o.inst.world, tol);
                    j.com_mm.unwrap_or(j.frame_mm) + 50.0 * j.inertia_rel.unwrap_or(0.0)
                }
                _ => frame_delta(&e.world, &o.inst.world).0,
            };
        }
    }
    let pairs = assign(&cost);
    let mut judged: Vec<(usize, usize, Judged)> = Vec::new();
    for (i, k) in pairs.iter().enumerate() {
        if let Some(k) = k {
            let (e, o) = (exp_leaves[i], obs_leaves[*k]);
            let j = match (ref_geo(&e.doc), o.geo.as_ref()) {
                (Some(rg), Some(og)) => judge(rg, &parts[&e.doc].verts, &e.world, og, &o.inst.world, tol),
                _ => {
                    let (mm, deg) = frame_delta(&e.world, &o.inst.world);
                    let frame_ok = mm <= tol.pos_mm && deg <= tol.angle_deg;
                    Judged { frame_mm: mm, frame_deg: deg, frame_ok, ok: frame_ok, method: "frame", reason: "no measurement to compare the geometry by".into(), ..Judged::default() }
                }
            };
            judged.push((i, *k, j));
        }
    }
    let matched_obs: std::collections::BTreeSet<usize> = pairs.iter().flatten().copied().collect();
    // Sub-assemblies: matched by name and frame.
    let exp_asm: Vec<&ExpInst> = expected.insts.iter().filter(|e| e.is_assembly).collect();
    let obs_asm: Vec<&Obs> = observed.iter().filter(|o| !o.inst.leaf).collect();
    let mut acost = vec![vec![FORBIDDEN; obs_asm.len()]; exp_asm.len()];
    for (i, e) in exp_asm.iter().enumerate() {
        for (k, o) in obs_asm.iter().enumerate() {
            if o.inst.comp_name == e.doc_name || name_root(o.inst.name()) == e.doc_name || name_root(o.inst.name()) == name_root(e.name()) {
                let (mm, deg) = frame_delta(&e.world, &o.inst.world);
                acost[i][k] = mm + deg;
            }
        }
    }
    let apairs = assign(&acost);

    // 4. The built designs of the parts.
    let mut file_of_doc: BTreeMap<String, String> = BTreeMap::new();
    for (i, k) in pairs.iter().enumerate() {
        if let Some(k) = k {
            if let Some(f) = links.get(&obs_leaves[*k].inst.comp) {
                file_of_doc.entry(exp_leaves[i].doc.clone()).or_insert_with(|| f.clone());
            }
        }
    }
    let mut measured_files: BTreeMap<String, Measured> = BTreeMap::new();
    let mut want: Vec<(String, String)> = Vec::new();
    for d in &part_docs {
        let file = file_of_doc.get(&d.id).cloned().or_else(|| {
            [inp.built.join("parts").join(format!("{}.sc", d.name)), inp.built.join(format!("{}.sc", d.name))].into_iter().find(|p| p.is_file()).map(|p| p.display().to_string())
        });
        if let Some(f) = file {
            want.push((d.id.clone(), f));
        }
    }
    let distinct: std::collections::BTreeSet<&String> = want.iter().map(|(_, f)| f).collect();
    for (n, f) in distinct.iter().enumerate() {
        log(&format!("built design {}/{}: {}", n + 1, distinct.len(), f));
        measured_files.insert((*f).clone(), measure_design(s, f));
    }
    for (doc, f) in &want {
        if let Some(p) = parts.get_mut(doc) {
            p.built_file = Some(f.clone());
            p.built = measured_files.get(f).cloned();
        }
    }

    // ---- Check 1: the part designs.
    let c1 = check_parts(bundle, &part_docs, &parts, &file_of_doc, tol);
    // ---- Check 2: placements.
    let c2 = check_placements(&expected, &exp_leaves, &obs_leaves, &judged, &pairs, &matched_obs, &exp_asm, &obs_asm, &apairs, &observed, tol);
    // ---- Check 3: names.
    let c3 = check_names(bundle, &part_docs, &parts, &exp_leaves, &obs_leaves, &pairs, &exp_asm, &obs_asm, &apairs, &observed, &tree);
    // ---- Check 4: overall.
    let c4 = check_overall(&expected, &exp_leaves, &obs_leaves, &exp_asm, &obs_asm, &parts, world_box, world.as_ref().err(), tol);
    // ---- Check 5: interference.
    let c5 = check_interference(clash, &body_volume, &judged, &exp_leaves, &obs_leaves, &closed_names, tol);

    report.checks = vec![c1, c2, c3, c4, c5];
    report.limits = limits(bundle, &expected, &parts, &world);
    report.seconds = t0.elapsed().as_secs_f64();
    Ok(report)
}

fn limits(bundle: &Bundle, expected: &Expected, parts: &BTreeMap<String, PartData>, world: &Result<Json, String>) -> Vec<String> {
    let mut l = Vec::new();
    let with_mass = bundle.documents.iter().filter(|d| d.kind == DocKind::Part && d.step.is_some() && d.mass.volume_mm3.is_some()).count();
    let total = bundle.documents.iter().filter(|d| d.kind == DocKind::Part && d.step.is_some()).count();
    if with_mass < total {
        l.push(format!(
            "The bundle carries Inventor's volume and range box for {with_mass} of {total} parts. For the others the reference is SolidCraft's own import of the bundle's STEP (the volume and box of what Inventor exported, as SolidCraft reads them). That tests everything the converter does to a part (sheet metal conversion, saving, linking) but not Inventor's own numbers; read it together with the STEP's declared unit, which is applied when the vertices are read."
        ));
    }
    let bad_verts = parts.values().filter(|p| p.verts_note.is_some()).count();
    if bad_verts > 0 {
        l.push(format!("{bad_verts} STEP file(s) could not be read for their vertices, so the lower bound of the box check is missing for them (the upper bound still applies)."));
    }
    if !expected.no_step.is_empty() {
        l.push("Parts whose STEP is missing in the bundle cannot be checked at all: nothing says what their geometry is.".into());
    }
    l.push("Constraints, joints and travel limits are not checked: this checks where things are, not how they move.".into());
    l.push("Orientation is judged by the part's inertia tensor and box as SolidCraft measures them in the world, besides the frame. A rotation that maps a part onto itself cannot be told from none by any measurement; such a pair is listed as equivalent with its frame difference.".into());
    l.push("Open bodies (surfaces) have no volume, centre of mass or inertia: they are judged by their box (and, with none, by the frame), and they are left out of the interference check.".into());
    if world.is_err() {
        l.push("SolidCraft's world measurement failed, so the overall box could not be compared.".into());
    }
    l
}

fn check_parts(bundle: &Bundle, part_docs: &[&Document], parts: &BTreeMap<String, PartData>, file_of_doc: &BTreeMap<String, String>, tol: &Tol) -> Check {
    let mut details = Vec::new();
    let mut rows = Vec::new();
    let (mut ok, mut failed, mut skipped, mut with_bundle) = (0usize, 0usize, 0usize, 0usize);
    let mut ratios: Vec<f64> = Vec::new();
    for d in part_docs {
        let p = &parts[&d.id];
        let mut problems: Vec<String> = Vec::new();
        let mut row = obj! {"part" => d.name.as_str(), "design" => p.built_file.clone()};
        if p.no_solid {
            skipped += 1;
            row.set("status", "not expected: the STEP holds no solid");
            rows.push(row);
            continue;
        }
        if let Some(e) = &p.reference.error {
            problems.push(format!("SolidCraft could not read the bundle's STEP: {e}"));
        }
        match (&p.built_file, &p.built) {
            (None, _) => problems.push("no saved design found (it is not linked into the assembly and there is no parts/<name>.sc)".into()),
            (Some(f), Some(m)) => {
                if let Some(e) = &m.error {
                    problems.push(format!("the design {f} cannot be opened or measured: {e}"));
                } else if let (Some(bg), Some(rg)) = (&m.geo, &p.reference.geo) {
                    row.set("built_volume_mm3", bg.volume);
                    row.set("step_volume_mm3", rg.volume);
                    if let (Some(bv), Some(rv)) = (bg.volume, rg.volume) {
                        let r = rel_diff(bv, rv);
                        row.set("volume_rel_vs_step", r);
                        if rv > 0.0 {
                            ratios.push(bv / rv);
                        }
                        if r > tol.volume_rel {
                            problems.push(format!("volume {} mm3 against {} mm3 from its STEP: {:.3}% off", fmt(bv), fmt(rv), r * 100.0));
                        }
                    } else if bg.volume.is_some() != rg.volume.is_some() {
                        problems.push("one of the built design and its STEP is a closed solid and the other is not".into());
                    }
                    if let (Some(bb), Some(rb)) = (bg.bbox, rg.bbox) {
                        let dmax = (bb.min - rb.min).norm().max((bb.max - rb.max).norm());
                        row.set("bbox_offset_mm", dmax);
                        row.set("built_bbox", box_json(&bb));
                        let ds = (bb.size() - rb.size()).norm();
                        if dmax > tol.bbox_mm.max(1e-5 * rb.diagonal()) {
                            problems.push(format!("box corners {} mm from the STEP's (size differs by {} mm)", fmt(dmax), fmt(ds)));
                        }
                    }
                    // Against what Inventor reported.
                    if let (Some(bv), Some(mv)) = (bg.volume, d.mass.volume_mm3) {
                        with_bundle += 1;
                        let r = rel_diff(bv, mv);
                        row.set("bundle_volume_mm3", mv);
                        row.set("volume_rel_vs_inventor", r);
                        if r > tol.bundle_volume_rel {
                            problems.push(format!("volume {} mm3 against Inventor's {} mm3: {:.2}% off", fmt(bv), fmt(mv), r * 100.0));
                        }
                    }
                    if let (Some(bb), Some(lo), Some(hi)) = (bg.bbox, d.mass.bbox_min, d.mass.bbox_max) {
                        let ib = Bbox { min: lo, max: hi };
                        let slack = tol.bundle_bbox_mm.max(tol.bundle_bbox_rel * ib.diagonal());
                        let ds = (bb.size() - ib.size()).norm();
                        row.set("bbox_size_delta_vs_inventor_mm", ds);
                        let per_axis = [(bb.size().x - ib.size().x).abs(), (bb.size().y - ib.size().y).abs(), (bb.size().z - ib.size().z).abs()];
                        if per_axis.iter().any(|x| *x > slack) {
                            problems.push(format!("box size {:?} against Inventor's range box {:?} (slack {} mm)", bb.size().to_array().map(|x| (x * 100.0).round() / 100.0), ib.size().to_array().map(|x| (x * 100.0).round() / 100.0), fmt(slack)));
                        }
                    }
                    if m.design_name.is_empty() {
                        row.set("design_name", Json::Null);
                    } else {
                        row.set("design_name", m.design_name.as_str());
                    }
                } else {
                    problems.push("the built design has no measurement".into());
                }
            }
            (Some(f), None) => problems.push(format!("design {f} was not measured")),
        }
        if let Some(f) = file_of_doc.get(&d.id) {
            let others: Vec<&&Document> = part_docs.iter().filter(|o| o.id != d.id && file_of_doc.get(&o.id) == Some(f)).collect();
            if !others.is_empty() {
                row.set("shares_design_with", others.iter().map(|o| o.name.clone()).collect::<Vec<_>>());
            }
        }
        if problems.is_empty() {
            ok += 1;
            row.set("status", "ok");
        } else {
            failed += 1;
            row.set("status", "FAIL");
            for p in &problems {
                details.push(format!("{}: {p}", d.name));
            }
            row.set("problems", problems);
        }
        rows.push(row);
    }
    // A systematic factor in every volume points at units, not at single parts.
    let mut factor_note = None;
    if ratios.len() >= 3 {
        let mut r = ratios.clone();
        r.sort_by(|a, b| a.total_cmp(b));
        let med = r[r.len() / 2];
        if (med - 1.0).abs() > 0.01 {
            let linear = med.cbrt();
            let known = [(25.4, "inches read as millimetres"), (1.0 / 25.4, "millimetres read as inches"), (10.0, "centimetres read as millimetres"), (0.1, "millimetres read as centimetres"), (1000.0, "metres read as millimetres"), (0.001, "millimetres read as metres")];
            let hint = known.iter().find(|(f, _)| rel_diff(*f, linear) < 0.01).map(|(_, w)| format!(" ({w})")).unwrap_or_default();
            let note = format!("A systematic volume factor: the median built/STEP volume is {med:.4}, a linear factor of {linear:.4}{hint}. That is a units problem, not a part problem.");
            details.insert(0, note.clone());
            factor_note = Some(note);
            failed += 1;
        }
    }
    let total = part_docs.len();
    let status = if failed > 0 { Status::Fail } else { Status::Pass };
    let summary = format!(
        "{ok} of {} part designs match (volume within {:.2}% and box within {} mm of SolidCraft's own import of the STEP{}); {failed} do not, {skipped} not expected (the STEP holds no solid){}",
        total - skipped,
        tol.volume_rel * 100.0,
        tol.bbox_mm,
        if with_bundle > 0 { format!("; {with_bundle} also against Inventor's volume within {:.1}%", tol.bundle_volume_rel * 100.0) } else { "; no mass in the bundle (no Inventor volume or box for any part), so nothing was compared against Inventor's own numbers".to_string() },
        factor_note.as_ref().map(|_| ", and a systematic factor").unwrap_or_default()
    );
    let _ = bundle;
    Check { id: 1, title: "Part designs: saved, volume, bounding box".into(), status, summary, details, data: obj! {"parts" => Json::Arr(rows), "systematic_factor" => factor_note} }
}

#[allow(clippy::too_many_arguments)]
fn check_placements(
    expected: &Expected,
    exp_leaves: &[&ExpInst],
    obs_leaves: &[&Obs],
    judged: &[(usize, usize, Judged)],
    pairs: &[Option<usize>],
    matched_obs: &std::collections::BTreeSet<usize>,
    exp_asm: &[&ExpInst],
    obs_asm: &[&Obs],
    apairs: &[Option<usize>],
    all_obs: &[Obs],
    tol: &Tol,
) -> Check {
    let mut details = Vec::new();
    let mut rows = Vec::new();
    let mut missing = Vec::new();
    for (i, p) in pairs.iter().enumerate() {
        if p.is_none() {
            let e = exp_leaves[i];
            missing.push(pathname(&e.names));
            details.push(format!("MISSING {} ({}): nothing built matches it", pathname(&e.names), e.doc_name));
        }
    }
    let mut extra = Vec::new();
    for (k, o) in obs_leaves.iter().enumerate() {
        if !matched_obs.contains(&k) {
            extra.push(pathname(&o.inst.names));
            details.push(format!("EXTRA {} ({}): built but not in the bundle", pathname(&o.inst.names), o.inst.comp_name));
        }
    }
    let (mut ok, mut equivalent, mut wrong) = (0usize, 0usize, 0usize);
    let mut worst_pos: f64 = 0.0;
    let mut by_kind: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut unmeasured = 0usize;
    for (i, k, j) in judged {
        let (e, o) = (exp_leaves[*i], obs_leaves[*k]);
        if o.geo.is_none() {
            unmeasured += 1;
            details.push(format!("NOT MEASURED {}: {}", pathname(&o.inst.names), o.error.as_deref().unwrap_or("SolidCraft gave no measurement")));
        }
        let mut row = obj! {
            "expected" => pathname(&e.names), "built" => pathname(&o.inst.names), "part" => e.doc_name.as_str(),
            "frame_mm" => j.frame_mm, "frame_deg" => j.frame_deg, "method" => j.method,
            "com_mm" => j.com_mm, "inertia_rel" => j.inertia_rel, "volume_rel" => j.volume_rel, "bbox_excess_mm" => j.bbox_excess_mm,
        };
        if j.ok {
            ok += 1;
            if j.equivalent {
                equivalent += 1;
                row.set("status", "equivalent");
                row.set("note", format!("geometry is right; the frame is {} mm and {:.2} deg from the bundle's", fmt(j.frame_mm), j.frame_deg));
            } else {
                row.set("status", "ok");
            }
        } else {
            wrong += 1;
            row.set("status", "MISPLACED");
            row.set("reason", j.reason.as_str());
            let d = j.com_mm.unwrap_or(j.frame_mm);
            worst_pos = worst_pos.max(d);
            by_kind.entry(e.doc_name.clone()).or_default().push(d);
            details.push(format!(
                "MISPLACED {} ({}): {}; frame {} mm and {:.2} deg from Inventor's{}",
                pathname(&o.inst.names),
                e.doc_name,
                j.reason,
                fmt(j.frame_mm),
                j.frame_deg,
                if o.inst.names != e.names { format!("; the bundle's name for it is {}", pathname(&e.names)) } else { String::new() }
            ));
        }
        rows.push(row);
    }
    // Sub-assemblies.
    let mut asm_rows = Vec::new();
    let mut asm_missing = 0usize;
    let mut asm_off = 0usize;
    for (i, p) in apairs.iter().enumerate() {
        let e = exp_asm[i];
        match p {
            None => {
                asm_missing += 1;
                details.push(format!("MISSING sub-assembly {} ({})", pathname(&e.names), e.doc_name));
                asm_rows.push(obj! {"expected" => pathname(&e.names), "status" => "MISSING"});
            }
            Some(k) => {
                let o = obs_asm[*k];
                let (mm, deg) = frame_delta(&e.world, &o.inst.world);
                let off = mm > tol.pos_mm.max(0.05) || deg > tol.angle_deg;
                if off {
                    asm_off += 1;
                }
                asm_rows.push(obj! {"expected" => pathname(&e.names), "built" => pathname(&o.inst.names), "frame_mm" => mm, "frame_deg" => deg, "status" => if off { "frame differs" } else { "ok" }});
            }
        }
    }
    let asm_extra = obs_asm.len().saturating_sub(apairs.iter().flatten().count());
    // Structure: each part, and each sub-assembly, sits in the assembly Inventor put it in (the
    // top level included).
    let by_path: BTreeMap<&[u64], &Obs> = all_obs.iter().map(|o| (o.inst.path.as_slice(), o)).collect();
    let mut wrong_parent = 0usize;
    let mut structure: Vec<(&ExpInst, &Obs)> = judged.iter().map(|(i, k, _)| (exp_leaves[*i], obs_leaves[*k])).collect();
    structure.extend(apairs.iter().enumerate().filter_map(|(i, k)| k.map(|k| (exp_asm[i], obs_asm[k]))));
    for (e, o) in structure {
        let built_parent: Option<String> = match o.inst.path.split_last() {
            Some((_, rest)) if !rest.is_empty() => by_path.get(rest).map(|p| p.inst.comp_name.clone()),
            _ => None,
        };
        let want_parent: Option<&str> = if e.names.len() > 1 { Some(e.parent_name.as_str()) } else { None };
        let same = match (&built_parent, want_parent) {
            (None, None) => true,
            (Some(b), Some(w)) => b == w || name_root(b) == w,
            _ => false,
        };
        if !same {
            wrong_parent += 1;
            details.push(format!(
                "STRUCTURE {}: it sits {} but Inventor has it {}",
                pathname(&o.inst.names),
                built_parent.map_or("at the top level".to_string(), |p| format!("in `{p}`")),
                want_parent.map_or("at the top level".to_string(), |p| format!("in `{p}`"))
            ));
        }
    }
    let n_exp = exp_leaves.len();
    let exactly_once = n_exp - missing.len();
    let pass = missing.is_empty() && extra.is_empty() && wrong == 0 && asm_missing == 0 && wrong_parent == 0 && unmeasured == 0;
    let kinds: Vec<String> = by_kind.iter().map(|(k, v)| format!("{k} x{} (up to {} mm)", v.len(), fmt(v.iter().cloned().fold(0.0, f64::max)))).collect();
    let summary = format!(
        "{exactly_once} of {n_exp} placed parts are present once; {} placed correctly ({equivalent} of them with another frame but the same geometry), {wrong} misplaced{}, {} missing, {} extra; sub-assemblies {} of {} present ({asm_off} with a different frame, {asm_extra} extra); {wrong_parent} in the wrong sub-assembly{}",
        ok,
        if wrong > 0 { format!(" (worst {} mm)", fmt(worst_pos)) } else { String::new() },
        missing.len(),
        extra.len(),
        exp_asm.len() - asm_missing,
        exp_asm.len(),
        if unmeasured > 0 { format!("; {unmeasured} could not be measured") } else { String::new() }
    );
    let _ = (expected, kinds.len());
    let mut data = obj! {
        "expected_parts" => n_exp, "present" => exactly_once, "ok" => ok, "equivalent_frame" => equivalent, "misplaced" => wrong,
        "missing" => missing.clone(), "extra" => extra.clone(), "unmeasured" => unmeasured,
        "assemblies_expected" => exp_asm.len(), "assemblies_missing" => asm_missing, "assemblies_frame_differs" => asm_off,
        "wrong_parent" => wrong_parent,
        "tolerance_mm" => tol.pos_mm, "tolerance_rel" => tol.pos_rel, "tolerance_deg" => tol.angle_deg, "tolerance_inertia_rel" => tol.inertia_rel,
    };
    data.set("placements", Json::Arr(rows));
    data.set("assemblies", Json::Arr(asm_rows));
    data.set("misplaced_by_part", Json::Arr(kinds.iter().map(|k| Json::from(k.as_str())).collect()));
    Check { id: 2, title: "Placements: every occurrence once, at its Inventor position and orientation".into(), status: if pass { Status::Pass } else { Status::Fail }, summary, details, data }
}

#[allow(clippy::too_many_arguments)]
fn check_names(
    bundle: &Bundle,
    part_docs: &[&Document],
    parts: &BTreeMap<String, PartData>,
    exp_leaves: &[&ExpInst],
    obs_leaves: &[&Obs],
    pairs: &[Option<usize>],
    exp_asm: &[&ExpInst],
    obs_asm: &[&Obs],
    apairs: &[Option<usize>],
    all_obs: &[Obs],
    tree: &Tree,
) -> Check {
    let mut details = Vec::new();
    let mut rows = Vec::new();
    let (mut same, mut renamed, mut renumbered) = (0usize, 0usize, 0usize);
    let mut pairs_all: Vec<(&ExpInst, &Obs)> = Vec::new();
    for (i, k) in pairs.iter().enumerate() {
        if let Some(k) = k {
            pairs_all.push((exp_leaves[i], obs_leaves[*k]));
        }
    }
    for (i, k) in apairs.iter().enumerate() {
        if let Some(k) = k {
            pairs_all.push((exp_asm[i], obs_asm[*k]));
        }
    }
    let _ = all_obs;
    for (e, o) in &pairs_all {
        if o.inst.names == e.names {
            same += 1;
            continue;
        }
        let kind = if o.inst.names.len() == e.names.len() && o.inst.names.iter().zip(&e.names).all(|(a, b)| a == b || name_root(a) == name_root(b)) {
            renumbered += 1;
            "occurrence number differs"
        } else {
            renamed += 1;
            "other name"
        };
        rows.push(obj! {"inventor" => pathname(&e.names), "solidcraft" => pathname(&o.inst.names), "part" => e.doc_name.as_str(), "kind" => kind});
        details.push(format!("occurrence `{}` is `{}` in SolidCraft ({kind})", pathname(&e.names), pathname(&o.inst.names)));
    }
    // Designs.
    let mut design_bad = 0usize;
    let mut design_rows = Vec::new();
    for d in part_docs {
        let p = &parts[&d.id];
        if p.no_solid {
            continue;
        }
        let built = p.built.as_ref().map(|m| m.design_name.clone()).unwrap_or_default();
        let file_stem = p.built_file.as_ref().and_then(|f| PathBuf::from(f).file_stem().map(|s| s.to_string_lossy().to_string())).unwrap_or_default();
        let ok = built == d.name;
        if !ok {
            design_bad += 1;
            details.push(format!("part `{}`: its saved design is named `{built}` (file `{file_stem}`)", d.name));
        }
        design_rows.push(obj! {"part" => d.name.as_str(), "design_name" => built.as_str(), "file" => p.built_file.clone(), "ok" => ok});
    }
    // Components of the assembly against the documents they place.
    let mut comp_bad = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for (e, o) in &pairs_all {
        if o.inst.comp_name != e.doc_name && seen.insert((e.doc_name.clone(), o.inst.comp_name.clone())) {
            comp_bad += 1;
            details.push(format!("the component for `{}` in the assembly is named `{}`", e.doc_name, o.inst.comp_name));
        }
    }
    let _ = (bundle, tree);
    let total = pairs_all.len();
    let pass = renamed + renumbered + design_bad + comp_bad == 0;
    let summary = format!(
        "{same} of {total} matched occurrences carry the Inventor name exactly; {renumbered} differ only in the occurrence number, {renamed} have another name; {} of {} saved designs carry the Inventor part name; {comp_bad} component name(s) differ from the part they place",
        design_rows.len() - design_bad,
        design_rows.len()
    );
    Check {
        id: 3,
        title: "Names: occurrences and designs carry the Inventor names".into(),
        status: if pass { Status::Pass } else { Status::Fail },
        summary,
        details,
        data: obj! {"occurrences_same" => same, "occurrences_renumbered" => renumbered, "occurrences_renamed" => renamed, "designs_wrong" => design_bad, "components_wrong" => comp_bad, "differences" => Json::Arr(rows), "designs" => Json::Arr(design_rows)},
    }
}

#[allow(clippy::too_many_arguments)]
fn check_overall(
    expected: &Expected,
    exp_leaves: &[&ExpInst],
    obs_leaves: &[&Obs],
    exp_asm: &[&ExpInst],
    obs_asm: &[&Obs],
    parts: &BTreeMap<String, PartData>,
    world_box: Option<Bbox>,
    world_err: Option<&String>,
    tol: &Tol,
) -> Check {
    let mut details = Vec::new();
    let mut fail = false;
    let count_ok = exp_leaves.len() == obs_leaves.len();
    if !count_ok {
        fail = true;
        details.push(format!("part count: the bundle places {} parts that can be built, SolidCraft holds {}", exp_leaves.len(), obs_leaves.len()));
    }
    let asm_ok = exp_asm.len() == obs_asm.len();
    if !asm_ok {
        fail = true;
        details.push(format!("sub-assembly count: the bundle has {}, SolidCraft {}", exp_asm.len(), obs_asm.len()));
    }
    // The expected world box: the union of every part's box placed by the bundle's matrices.
    let (mut upper, mut lower): (Option<Bbox>, Option<Bbox>) = (None, None);
    for e in exp_leaves {
        let Some(p) = parts.get(&e.doc) else { continue };
        let Some(rb) = p.reference.geo.as_ref().and_then(|g| g.bbox) else { continue };
        let u = transformed_box(&rb, &e.world);
        let l = if crate::judge::axis_aligned(&e.world) || p.verts.is_empty() { u } else { Bbox::of(p.verts.iter().map(|v| e.world.point(*v))) };
        for (acc, b) in [(&mut upper, u), (&mut lower, l)] {
            match acc {
                Some(a) => {
                    a.add(b.min);
                    a.add(b.max);
                }
                None => *acc = Some(b),
            }
        }
    }
    let mut box_line = String::from("overall box not compared");
    let mut data_box = Json::Null;
    if let (Some(w), Some(u), Some(l)) = (world_box, upper, lower) {
        let diag = u.diagonal();
        let t = tol.pos_mm.max(tol.pos_rel * diag);
        let mut worst: f64 = 0.0;
        for (o_min, o_max, u_min, u_max, l_min, l_max) in [
            (w.min.x, w.max.x, u.min.x, u.max.x, l.min.x, l.max.x),
            (w.min.y, w.max.y, u.min.y, u.max.y, l.min.y, l.max.y),
            (w.min.z, w.max.z, u.min.z, u.max.z, l.min.z, l.max.z),
        ] {
            worst = worst.max(u_min - o_min).max(o_max - u_max).max(o_min - l_min).max(l_max - o_max);
        }
        let excess = (worst - t).max(0.0);
        let exact = (u.min - l.min).norm().max((u.max - l.max).norm()) < 1e-9;
        box_line = format!(
            "SolidCraft's world box {} .. {}; from the bundle {} .. {}{}; {} mm outside",
            fmt3(w.min),
            fmt3(w.max),
            fmt3(l.min),
            fmt3(l.max),
            if exact { String::new() } else { format!(" (at most {} .. {}: curved parts turned off the axes are only bounded)", fmt3(u.min), fmt3(u.max)) },
            fmt(excess)
        );
        if excess > 0.0 {
            fail = true;
            details.push(format!("world box is {} mm outside what the bundle's placements give: {box_line}", fmt(excess)));
        }
        data_box = obj! {"solidcraft_min" => w.min.to_array(), "solidcraft_max" => w.max.to_array(), "bundle_min" => l.min.to_array(), "bundle_max" => l.max.to_array(), "bundle_upper_min" => u.min.to_array(), "bundle_upper_max" => u.max.to_array(), "excess_mm" => excess, "exact" => exact};
    } else if let Some(e) = world_err {
        details.push(format!("SolidCraft could not measure the world: {e}"));
        fail = true;
    }
    let summary = format!(
        "parts {} of {} placed in the bundle ({} left out of the count: no STEP or no solid); sub-assemblies {} of {}; {box_line}",
        obs_leaves.len(),
        exp_leaves.len(),
        expected.insts.iter().filter(|e| !e.is_assembly).count() - exp_leaves.len(),
        obs_asm.len(),
        exp_asm.len()
    );
    Check { id: 4, title: "Overall: part count and world bounding box".into(), status: if fail { Status::Fail } else { Status::Pass }, summary, details, data: obj! {"expected_parts" => exp_leaves.len(), "built_parts" => obs_leaves.len(), "expected_assemblies" => exp_asm.len(), "built_assemblies" => obs_asm.len(), "box" => data_box} }
}

fn fmt3(v: V3) -> String {
    format!("({}, {}, {})", fmt(v.x), fmt(v.y), fmt(v.z))
}

fn check_interference(
    clash: Option<Result<Json, String>>,
    body_volume: &BTreeMap<String, f64>,
    judged: &[(usize, usize, Judged)],
    exp_leaves: &[&ExpInst],
    obs_leaves: &[&Obs],
    closed: &[String],
    tol: &Tol,
) -> Check {
    let title = "Interference (information)".to_string();
    let Some(r) = clash else {
        return Check { id: 5, title, status: Status::Info, summary: "not run: this SolidCraft has no interference command in the profile, or no closed bodies".into(), details: vec![], data: Json::Null };
    };
    let j = match r {
        Ok(j) => j,
        Err(e) => return Check { id: 5, title, status: Status::Info, summary: format!("the interference check failed: {e}"), details: vec![], data: Json::Null },
    };
    // A part is trusted where check 2 found it at Inventor's position: an overlap between two such
    // parts is in the Inventor model too.
    let placed_ok: std::collections::BTreeSet<String> = judged.iter().filter(|(_, _, j)| j.ok).map(|(_, k, _)| pathname(&obs_leaves[*k].inst.names)).collect();
    let _ = exp_leaves;
    let mut pairs = Vec::new();
    let mut details = Vec::new();
    let mut suspect = 0usize;
    let mut source_clash = 0usize;
    let mut rows = Vec::new();
    for p in j.arr_of("interferences") {
        let (a, b) = (p.str_of("a").unwrap_or_default(), p.str_of("b").unwrap_or_default());
        let v = p.f64_of("volume_mm3").unwrap_or(0.0);
        let smaller = [body_volume.get(a), body_volume.get(b)].into_iter().flatten().cloned().fold(f64::INFINITY, f64::min);
        let frac = if smaller.is_finite() && smaller > 0.0 { v / smaller } else { 0.0 };
        let place = |k: &str| p.get(k).map(|x| x.arr_of("path").iter().filter_map(|s| s.as_str().map(str::to_string)).collect::<Vec<_>>().join(" > ")).unwrap_or_default();
        let (pa, pb) = (place("a_part"), place("b_part"));
        let (pa, pb) = (if pa.is_empty() { a.to_string() } else { pa }, if pb.is_empty() { b.to_string() } else { pb });
        let both_ok = placed_ok.contains(&pa) && placed_ok.contains(&pb);
        let big = frac >= tol.interference_fail_fraction;
        let bad = big && !both_ok;
        if bad {
            suspect += 1;
        } else if big {
            source_clash += 1;
        }
        pairs.push((v, pa.clone(), pb.clone(), frac, bad, big && both_ok));
        rows.push(obj! {"a" => a, "b" => b, "a_path" => pa, "b_path" => pb, "volume_mm3" => v, "fraction_of_smaller" => frac, "large" => big, "suspect" => bad, "both_placed_as_inventor_has_them" => both_ok});
    }
    pairs.sort_by(|x, y| y.0.total_cmp(&x.0));
    for (v, pa, pb, frac, bad, source) in pairs.iter().take(40) {
        details.push(format!(
            "{}{pa} with {pb}: {} mm3, {:.1}% of the smaller{}",
            if *bad { "SUSPECT " } else { "" },
            fmt(*v),
            frac * 100.0,
            if *source { "; both are where Inventor has them, so this overlap is in the Inventor model too" } else { "" }
        ));
    }
    let unchecked = j.arr_of("unchecked").len();
    let exceeded = j.bool_of("budget_exceeded").unwrap_or(false);
    let summary = format!(
        "{} interfering pair(s) among {} closed bodies, largest {} mm3; {} where the overlap is at least {:.0}% of the smaller body ({source_clash} of them between parts that check 2 found where Inventor has them, so the clash is in the source; {suspect} suspect, involving a part that is not); {unchecked} pair(s) not checked{}. The source assembly may have clashes of its own.",
        pairs.len(),
        closed.len(),
        pairs.first().map(|p| fmt(p.0)).unwrap_or_else(|| "0".into()),
        suspect + source_clash,
        tol.interference_fail_fraction * 100.0,
        if exceeded { " (the time budget ran out)" } else { "" }
    );
    Check {
        id: 5,
        title,
        status: if suspect > 0 { Status::Fail } else { Status::Info },
        summary,
        details,
        data: obj! {"pairs" => Json::Arr(rows), "unchecked" => unchecked, "budget_exceeded" => exceeded, "closed_bodies" => closed.len(), "suspect" => suspect, "source_clash" => source_clash},
    }
}
