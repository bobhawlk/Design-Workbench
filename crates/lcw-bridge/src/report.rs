//! The conversion report: what was found and decided, and what happened in SolidCraft.

use lcw_analyze::joints::Source;
use lcw_analyze::Analysis;
use lcw_json::{obj, Json};
use lcw_solidcraft::emit::describe_pattern;
use lcw_solidcraft::run::{DesignResult, Status};

fn name_of(a: &Analysis, path: &[usize]) -> String {
    a.model.path_ids(path).last().cloned().unwrap_or_default()
}

pub fn markdown(a: &Analysis, runs: &[DesignResult], probe: &[String]) -> String {
    let m = &a.model;
    let mut s = String::new();
    // Parts whose `sheet.convert` step failed: they stayed solid in SolidCraft even though the
    // plan called for sheet metal.
    let sheet_failed: std::collections::BTreeSet<&str> = runs
        .iter()
        .filter(|r| r.failures().any(|f| f.label.starts_with("make ") && f.label.ends_with(" sheet metal")))
        .map(|r| r.name.as_str())
        .collect();
    let root = &m.defs[m.root];
    let parts_used: std::collections::BTreeSet<usize> = a.instances.iter().filter(|x| !m.defs[x.def].assembly).map(|x| x.def).collect();
    s.push_str(&format!("# {} → SolidCraft\n\n", root.name));
    s.push_str(&format!("Source: {} ({})  \n", m.source_file, m.source_app));
    s.push_str(&format!(
        "{} placed parts, {} distinct part designs, {} sub-assemblies, {} sheet metal parts, {} joints, {} constraints.\n\n",
        a.instances.iter().filter(|x| !m.defs[x.def].assembly).count(),
        parts_used.len(),
        a.instances.iter().filter(|x| m.defs[x.def].assembly).count(),
        a.sheets.len(),
        a.joints.joints.len(),
        a.joints.constraints.len()
    ));
    if !probe.is_empty() {
        s.push_str("## SolidCraft compatibility\n\n");
        for p in probe {
            s.push_str(&format!("- {p}\n"));
        }
        s.push('\n');
    }
    s.push_str("## Parts\n\n| Part | Placed | Becomes | Notes |\n|---|---|---|---|\n");
    for d in &parts_used {
        let def = &m.defs[*d];
        let count = a.instances.iter().filter(|x| x.def == *d).count();
        let (becomes, notes) = match a.sheets.get(d) {
            Some(sp) => (
                format!("sheet metal, {} ({} bend{})", sp.rule, sp.bends.len(), if sp.bends.len() == 1 { "" } else { "s" }),
                sp.notes.join("; "),
            ),
            None => ("solid".to_string(), String::new()),
        };
        let (becomes, notes) = if sheet_failed.contains(def.name.as_str()) {
            (
                "solid (sheet metal conversion failed)".to_string(),
                if notes.is_empty() { "see Build in SolidCraft below".to_string() } else { format!("{notes}; see Build in SolidCraft below") },
            )
        } else {
            (becomes, notes)
        };
        let mirror = a.mirrors.iter().find(|x| x.part == *d).map(|x| format!("mirror image of {}", m.defs[x.of].name)).unwrap_or_default();
        let notes = [notes, mirror].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("; ");
        s.push_str(&format!("| {} | {} | {} | {} |\n", def.name.replace('|', "/"), count, becomes, notes.replace('|', "/")));
    }
    if !a.merged.is_empty() || !a.copies.is_empty() {
        s.push_str("\n## Repeated parts\n\n");
        for (p, c) in &a.merged {
            s.push_str(&format!("- **{}** is the same part as **{}**: one component is used for both.\n", m.defs[*p].name, m.defs[*c].name));
        }
        for c in &a.copies {
            s.push_str(&format!("- **{}** is identical to **{}** (kept separate: the names differ; `--merge all` joins them).\n", m.defs[c.part].name, m.defs[c.of].name));
        }
    }
    if !a.patterns.is_empty() {
        s.push_str("\n## Patterns\n\n");
        for p in &a.patterns {
            s.push_str(&format!("- {} in {}: {}{}\n", p.name, m.defs[p.assembly].name, describe_pattern(&p.found.kind), if p.from_inventor { " (Inventor pattern)" } else { " (recognised)" }));
        }
    }
    if !a.symmetry.is_empty() {
        s.push_str("\n## Symmetry\n\n");
        for p in &a.symmetry {
            s.push_str(&format!(
                "- Mirror plane through ({:.2}, {:.2}, {:.2}) with normal ({:.3}, {:.3}, {:.3}): {} pair(s) of parts.\n",
                p.point.x, p.point.y, p.point.z, p.normal.x, p.normal.y, p.normal.z, p.pairs.len()
            ));
        }
    }
    s.push_str("\n## Joints\n\n");
    if !a.joints.ground.is_empty() {
        s.push_str(&format!("Grounded: {}.\n\n", a.joints.ground.iter().map(|g| name_of(a, g)).collect::<Vec<_>>().join(", ")));
    }
    if !a.joints.joints.is_empty() {
        s.push_str("| Joint | Type | Fixed side | Moving side | From |\n|---|---|---|---|---|\n");
        for j in &a.joints.joints {
            let from = match &j.source {
                Source::InventorJoint => "Inventor joint".to_string(),
                Source::InventorConstraint => "Inventor constraint".to_string(),
                Source::Inferred(r) => format!("geometry: {r}"),
            };
            s.push_str(&format!("| {} | {} | {} | {} | {} |\n", j.name, j.kind.name(), name_of(a, &j.a), name_of(a, &j.b), from));
        }
    }
    if !a.joints.constraints.is_empty() {
        s.push_str(&format!("\n{} Inventor constraint(s) carried over as SolidCraft constraints.\n", a.joints.constraints.len()));
    }
    if !a.joints.rigid_groups.is_empty() {
        s.push_str(&format!("\n{} group(s) of parts held rigidly together (fasteners, spacers, bolted parts).\n", a.joints.rigid_groups.len()));
    }
    let notes: Vec<&String> = a.notes.iter().chain(m.warnings.iter()).collect();
    if !notes.is_empty() {
        s.push_str("\n## Notes\n\n");
        for n in notes {
            s.push_str(&format!("- {n}\n"));
        }
    }
    if !runs.is_empty() {
        s.push_str("\n## Build in SolidCraft\n\n");
        for r in runs {
            let failed: Vec<_> = r.failures().collect();
            let fallbacks = r.steps.iter().filter(|x| matches!(x.status, Status::Fallback(_))).count();
            s.push_str(&format!(
                "- **{}** → `{}`: {}{}{}\n",
                r.name,
                r.file,
                if r.saved { "saved" } else { "NOT saved" },
                if failed.is_empty() { String::new() } else { format!(", {} step(s) failed", failed.len()) },
                if fallbacks == 0 { String::new() } else { format!(", {fallbacks} used a fallback") }
            ));
            for f in failed.iter().take(20) {
                if let Status::Failed(e) = &f.status {
                    s.push_str(&format!("  - {}: {}\n", f.label, e.lines().next().unwrap_or("")));
                }
            }
        }
    }
    s
}

pub fn json(a: &Analysis, runs: &[DesignResult]) -> Json {
    let m = &a.model;
    obj! {
        "schema" => "lcw-report/1",
        "source" => m.source_file.as_str(),
        "sheets" => Json::Arr(a.sheets.iter().map(|(d, sp)| obj!{"part" => m.defs[*d].name.as_str(), "rule" => sp.rule.as_str(), "thickness" => sp.thickness, "bend_radius" => sp.bend_radius, "k_factor" => sp.k_factor, "k_assumed" => sp.k_assumed, "bends" => sp.bends.len()}).collect()),
        "joints" => Json::Arr(a.joints.joints.iter().map(|j| obj!{"name" => j.name.as_str(), "type" => j.kind.name(), "a" => name_of(a, &j.a), "b" => name_of(a, &j.b), "origin" => Json::from(j.origin.to_array()), "axis" => Json::from(j.z.to_array())}).collect()),
        "patterns" => Json::Arr(a.patterns.iter().map(|p| obj!{"name" => p.name.as_str(), "kind" => describe_pattern(&p.found.kind), "members" => p.found.members.len()}).collect()),
        "notes" => a.notes.clone(),
        "builds" => Json::Arr(runs.iter().map(|r| obj!{
            "design" => r.name.as_str(), "file" => r.file.as_str(), "saved" => r.saved,
            "steps" => Json::Arr(r.steps.iter().map(|s| {
                let (status, detail) = match &s.status {
                    Status::Done => ("done", String::new()),
                    Status::Fallback(o) => ("fallback", o.clone()),
                    Status::Skipped(w) => ("skipped", w.clone()),
                    Status::Failed(e) => ("failed", e.clone()),
                };
                obj!{"step" => s.label.as_str(), "status" => status, "detail" => detail}
            }).collect()),
        }).collect()),
    }
}
