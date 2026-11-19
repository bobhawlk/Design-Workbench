//! The conversion report: what was planned (found and decided), and, separately, what actually
//! happened in SolidCraft, step group by step group, with every failure's own error text.

use std::collections::BTreeMap;

use lcw_analyze::Analysis;
use lcw_json::{obj, Json};
use lcw_solidcraft::emit::describe_pattern;
use lcw_solidcraft::run::{DesignResult, Status, StepResult};

fn name_of(a: &Analysis, path: &[usize]) -> String {
    a.model.path_ids(path).last().cloned().unwrap_or_default()
}

fn cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

/// What became of a planned part, from its design's build.
fn built_as(r: Option<&DesignResult>, planned_sheet: bool) -> String {
    let Some(r) = r else { return "not built".into() };
    if !r.saved {
        return match &r.session_lost {
            Some(_) => "NOT built (the SolidCraft session was lost)".into(),
            None => "NOT saved".into(),
        };
    }
    let convert = r.steps.iter().find(|s| s.op == "sheet_convert");
    match (planned_sheet, convert.map(|c| &c.status)) {
        (true, Some(Status::Done)) => "sheet metal".into(),
        (true, Some(Status::Fallback(how))) => format!("sheet metal, {how}"),
        (true, Some(Status::Failed(_))) => "solid (sheet metal conversion failed)".into(),
        (true, Some(s)) => format!("solid (sheet metal {}: {})", s.name(), s.detail()),
        (true, None) => "solid (no sheet metal step)".into(),
        (false, _) => {
            if r.failures().next().is_some() {
                "solid, with failed steps".into()
            } else {
                "solid".into()
            }
        }
    }
}

pub fn markdown(a: &Analysis, runs: &[DesignResult], probe: &[String], events: &[String]) -> String {
    let m = &a.model;
    let mut s = String::new();
    let root = &m.defs[m.root];
    let parts_used: std::collections::BTreeSet<usize> = a.instances.iter().filter(|x| !m.defs[x.def].assembly).map(|x| x.def).collect();
    s.push_str(&format!("# {} → SolidCraft\n\n", root.name));
    s.push_str(&format!("Source: {} ({})  \n", m.source_file, m.source_app));
    s.push_str(&format!(
        "Planned: {} placed parts, {} distinct part designs, {} sub-assemblies, {} sheet metal parts, {} joints, {} constraints.\n\n",
        a.instances.iter().filter(|x| !m.defs[x.def].assembly).count(),
        parts_used.len(),
        a.instances.iter().filter(|x| m.defs[x.def].assembly).count(),
        a.sheets.len(),
        a.joints.joints.len(),
        a.joints.constraints.len()
    ));
    if runs.is_empty() {
        s.push_str("Nothing was built (analysis or dry run): everything below is the plan.\n\n");
    } else {
        s.push_str(&result_summary(runs));
        s.push_str(&mechanism_section(a, runs));
    }
    if !probe.is_empty() {
        s.push_str("## SolidCraft compatibility\n\n");
        for p in probe {
            s.push_str(&format!("- {p}\n"));
        }
        s.push('\n');
    }
    let by_name: BTreeMap<&str, &DesignResult> = runs.iter().map(|r| (r.name.as_str(), r)).collect();
    if runs.is_empty() {
        s.push_str("## Parts (planned)\n\n| Part | Placed | Planned | Notes |\n|---|---|---|---|\n");
    } else {
        s.push_str("## Parts: planned and built\n\n| Part | Placed | Planned | Built | Notes |\n|---|---|---|---|---|\n");
    }
    for d in &parts_used {
        let def = &m.defs[*d];
        let count = a.instances.iter().filter(|x| x.def == *d).count();
        let (planned, notes) = match a.sheets.get(d) {
            Some(sp) => (format!("sheet metal, {} ({} bend{})", sp.rule, sp.bends.len(), if sp.bends.len() == 1 { "" } else { "s" }), sp.notes.join("; ")),
            None => ("solid".to_string(), String::new()),
        };
        let mirror = a.mirrors.iter().find(|x| x.part == *d).map(|x| format!("mirror image of {}", m.defs[x.of].name)).unwrap_or_default();
        let notes = [notes, mirror].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("; ");
        if runs.is_empty() {
            s.push_str(&format!("| {} | {} | {} | {} |\n", cell(&def.name), count, cell(&planned), cell(&notes)));
        } else {
            let built = built_as(by_name.get(def.name.as_str()).copied(), a.sheets.contains_key(d));
            s.push_str(&format!("| {} | {} | {} | {} | {} |\n", cell(&def.name), count, cell(&planned), cell(&built), cell(&notes)));
        }
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
        s.push_str("\n## Patterns (planned)\n\n");
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
    s.push_str("\n## Joints (planned)\n\n");
    if !a.joints.ground.is_empty() {
        s.push_str(&format!("Grounded: {}.\n\n", a.joints.ground.iter().map(|g| name_of(a, g)).collect::<Vec<_>>().join(", ")));
    }
    if !a.joints.joints.is_empty() {
        s.push_str("| Joint | Type | Fixed side | Moving side | From |\n|---|---|---|---|---|\n");
        for j in &a.joints.joints {
            s.push_str(&format!("| {} | {} | {} | {} | {} |\n", cell(&j.name), j.kind.name(), cell(&name_of(a, &j.a)), cell(&name_of(a, &j.b)), cell(&j.basis())));
        }
    }
    if !a.joints.constraints.is_empty() {
        s.push_str(&format!("\n{} Inventor constraint(s) to carry over as SolidCraft constraints (their geometry was complete and lands on the parts' faces).\n", a.joints.constraints.len()));
    }
    if !a.joints.rigid_groups.is_empty() {
        s.push_str(&format!("\n{} group(s) of parts held rigidly together:\n\n| Held to | Parts | Why |\n|---|---|---|\n", a.joints.rigid_groups.len()));
        for g in &a.joints.rigid_groups {
            let mut why: BTreeMap<String, usize> = BTreeMap::new();
            for w in g.why.iter().skip(1) {
                *why.entry(w.clone()).or_default() += 1;
            }
            let text: Vec<String> = why.iter().map(|(w, n)| if *n > 1 { format!("{n} × {w}") } else { w.clone() }).collect();
            s.push_str(&format!("| {} | {} | {} |\n", cell(&name_of(a, &g.members[0])), g.members.len() - 1, cell(&text.join("; "))));
        }
    }
    if !a.joints.evidence.is_empty() {
        let ev = &a.joints.evidence;
        let count = |p: &str| ev.iter().filter(|e| e.outcome.starts_with(p)).count();
        s.push_str(&format!(
            "\n### Inventor constraints used as evidence\n\n{} Inventor constraint(s) and joint(s) were not made into SolidCraft constraints, because their exported geometry is incomplete or does not land on the parts (the reason is listed). They were used as evidence: {} agree with a rigid group, {} with a joint, {} decided what a part with no joint is held to, {} were not used, {} name no part with geometry.\n\n",
            ev.len(), count("agrees: the parts"), count("agrees: `"), count("used:"), count("not used"), count("names no part")
        ));
        s.push_str("| Constraint | In | Kind | Between | Why not a SolidCraft constraint | Outcome |\n|---|---|---|---|---|---|\n");
        for e in ev {
            s.push_str(&format!("| {} | {} | {} | {} / {} | {} | {} |\n", cell(&e.name), cell(&e.assembly), cell(&e.kind), cell(&name_of(a, &e.a)), cell(&name_of(a, &e.b)), cell(&e.why_not), cell(&e.outcome)));
        }
    }
    let notes: Vec<&String> = a.notes.iter().chain(m.warnings.iter()).collect();
    if !notes.is_empty() {
        s.push_str("\n## Notes\n\n");
        for n in notes {
            s.push_str(&format!("- {n}\n"));
        }
    }
    if !runs.is_empty() {
        s.push_str(&build_section(runs, events));
    }
    s
}

/// The top of the report: what was achieved against what was planned, over the whole run.
fn result_summary(runs: &[DesignResult]) -> String {
    let mut s = String::from("## Result\n\n");
    let saved = runs.iter().filter(|r| r.saved).count();
    let ms: f64 = runs.iter().map(|r| r.ms).sum();
    s.push_str(&format!("{saved} of {} designs saved, in {:.1} min. ", runs.len(), ms / 60000.0));
    let failed: usize = runs.iter().map(|r| r.count("failed")).sum();
    let not_run: usize = runs.iter().map(|r| r.count("not run")).sum();
    s.push_str(&format!("{failed} step(s) failed and {not_run} were not run; details under *Build in SolidCraft*.\n\n"));
    s.push_str("| Step group | Planned | Done | By fallback | Skipped | Failed | Not run |\n|---|---|---|---|---|---|---|\n");
    let mut groups: Vec<(String, [usize; 6])> = Vec::new();
    for st in runs.iter().flat_map(|r| &r.steps) {
        let i = match groups.iter().position(|(g, _)| *g == st.group) {
            Some(i) => i,
            None => {
                groups.push((st.group.clone(), [0; 6]));
                groups.len() - 1
            }
        };
        let c = &mut groups[i].1;
        c[0] += 1;
        c[match st.status {
            Status::Done => 1,
            Status::Fallback(_) => 2,
            Status::Skipped(_) => 3,
            Status::Failed(_) => 4,
            Status::NotRun(_) => 5,
        }] += 1;
    }
    for (g, c) in &groups {
        s.push_str(&format!("| {} | {} | {} | {} | {} | {} | {} |\n", cell(g), c[0], c[1], c[2], c[3], c[4], c[5]));
    }
    // Limits: what each joint ended with.
    let limits: Vec<&StepResult> = runs.iter().flat_map(|r| &r.steps).filter(|x| x.op == "auto_limits").collect();
    if !limits.is_empty() {
        let set = limits.iter().filter(|x| x.status.achieved()).count();
        let free = limits.iter().filter(|x| matches!(&x.status, Status::Skipped(w) if w.contains("nothing stops") || w.contains("turns freely"))).count();
        s.push_str(&format!(
            "\nJoint limits: {} planned, {set} set, {free} free (nothing stops the joint), {} not set (unknown or failed; listed below).\n",
            limits.len(),
            limits.len() - set - free
        ));
    }
    s.push('\n');
    s
}

/// Per joint: where it came from, whether SolidCraft made it, its limits, and whether the check
/// could drive it; then the mechanism's degrees of freedom, conflicts and floating chains.
fn mechanism_section(a: &Analysis, runs: &[DesignResult]) -> String {
    let Some(asm) = runs.last() else { return String::new() };
    let mut s = String::from("## Mechanism\n\n");
    let check = asm.steps.iter().find(|x| x.op == "verify_joints");
    let data = check.map(|c| c.data.clone()).unwrap_or_default();
    let checked = data.get("joints").and_then(Json::as_array).map(<[Json]>::to_vec).unwrap_or_default();
    match check.map(|c| &c.status) {
        Some(Status::Done) => s.push_str(&format!("SolidCraft's check of the saved assembly: {}.\n\n", check.map_or("", |c| c.note.as_str()))),
        Some(st) => s.push_str(&format!("The mechanism check did not run to the end ({}: {}).\n\n", st.name(), st.detail())),
        None => s.push_str("The mechanism check was not run.\n\n"),
    }
    if !a.joints.joints.is_empty() {
        s.push_str("| Joint | Type | From | Made | Limits | Check |\n|---|---|---|---|---|---|\n");
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let total: BTreeMap<&str, usize> = a.joints.joints.iter().fold(BTreeMap::new(), |mut m, j| {
        *m.entry(j.name.as_str()).or_insert(0) += 1;
        m
    });
    for j in &a.joints.joints {
        let k = seen.entry(j.name.as_str()).or_insert(0);
        *k += 1;
        let name = if total[j.name.as_str()] > 1 { format!("{} {}", j.name, k) } else { j.name.clone() };
        let made = asm.steps.iter().find(|x| x.group == "joints" && x.label.starts_with(&format!("{} {} (", j.kind.name(), name)));
        let made_text = match made.map(|x| &x.status) {
            Some(Status::Done) => "yes".to_string(),
            Some(st) => format!("{}: {}", st.name(), st.detail()),
            None => "not attempted (see notes)".to_string(),
        };
        let lim = asm.steps.iter().find(|x| x.op == "auto_limits" && x.label == format!("limit {name}"));
        let c = checked.iter().find(|c| c.str_of("joint") == Some(name.as_str()));
        let lim_text = match (c.and_then(|c| c.get("limits")).and_then(Json::as_array), lim.map(|x| &x.status)) {
            (Some(l), _) if l.len() == 2 => format!("{} to {} {}", fmt_num(l[0].as_f64()), fmt_num(l[1].as_f64()), c.and_then(|c| c.str_of("unit")).unwrap_or("")),
            (_, Some(Status::Skipped(w))) => w.clone(),
            (_, Some(st)) if !st.achieved() => format!("{}: {}", st.name(), st.detail()),
            _ => "none".to_string(),
        };
        let check_text = match c {
            Some(c) => {
                let mut t = c.str_of("result").unwrap_or("").to_string();
                if let Some(w) = c.str_of("why") {
                    t.push_str(&format!(": {w}"));
                }
                if let Some(l) = c.get("stopped_by_loop").and_then(|l| l.str_of("joint")) {
                    t.push_str(&format!(" (a loop through `{l}` stops it)"));
                }
                t
            }
            None => "-".to_string(),
        };
        s.push_str(&format!("| {} | {} | {} | {} | {} | {} |\n", cell(&name), j.kind.name(), cell(&j.basis()), cell(&made_text), cell(&lim_text), cell(&check_text)));
    }
    let list = |k: &str| data.get(k).and_then(Json::as_array).map(<[Json]>::to_vec).unwrap_or_default();
    let conflicts = list("conflicts");
    if !conflicts.is_empty() {
        s.push_str("\nConflicting joints (SolidCraft cannot place everything they ask for at once):\n\n");
        for c in &conflicts {
            s.push_str(&format!("- `{}`: {}\n", c.str_of("joint").unwrap_or("?"), c.str_of("why").unwrap_or("")));
        }
    }
    for e in list("errors") {
        s.push_str(&format!("- joint error: `{}`: {}\n", e.str_of("joint").unwrap_or("?"), e.str_of("why").unwrap_or("")));
    }
    let floating = list("floating");
    if !floating.is_empty() {
        s.push_str("\nFloating (joined to nothing grounded, so SolidCraft does not place them):\n\n");
        for f in &floating {
            let occ: Vec<String> = f.get("occurrences").and_then(Json::as_array).unwrap_or(&[]).iter().filter_map(|o| o.as_str().map(str::to_string)).collect();
            s.push_str(&format!("- {}\n", cell(&occ.join(", "))));
        }
    }
    s.push('\n');
    s
}

fn fmt_num(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{:.2}", x),
        None => "?".into(),
    }
}

fn indent_block(text: &str) -> String {
    let mut out = String::from("    ```\n");
    for l in text.lines() {
        out.push_str(&format!("    {l}\n"));
    }
    out.push_str("    ```\n");
    out
}

/// What happened in SolidCraft, per design: each failure with SolidCraft's own error, each
/// skipped and not-run step with its reason, the fallbacks used, and where the time went.
pub fn build_section(runs: &[DesignResult], events: &[String]) -> String {
    let mut s = String::from("\n## Build in SolidCraft\n\n");
    for e in events {
        s.push_str(&format!("- {e}\n"));
    }
    if !events.is_empty() {
        s.push('\n');
    }
    for r in runs {
        s.push_str(&format!("### {}\n\n`{}`: {}\n\n", r.name, r.file, r.summary()));
        if let Some(l) = &r.session_lost {
            s.push_str(&format!("The SolidCraft session was given up while building this design: {l}\n\n"));
        }
        if let Some(why) = &r.stopped {
            s.push_str(&format!("Stopped early: {}.\n\n", why.trim_start_matches("not run: ")));
        }
        // Per group, only where something did not simply go through.
        let mut groups: Vec<&str> = Vec::new();
        for st in &r.steps {
            if !groups.contains(&st.group.as_str()) {
                groups.push(&st.group);
            }
        }
        for g in groups {
            let in_g: Vec<&StepResult> = r.steps.iter().filter(|x| x.group == g).collect();
            if in_g.iter().all(|x| x.status == Status::Done && x.note.is_empty()) {
                continue;
            }
            let done = in_g.iter().filter(|x| x.status.achieved()).count();
            s.push_str(&format!("- **{g}**: {done} of {} done\n", in_g.len()));
            for x in &in_g {
                match &x.status {
                    Status::Done if !x.note.is_empty() => s.push_str(&format!("  - done, note: {}: {}\n", x.label, x.note)),
                    Status::Done => {}
                    Status::Fallback(how) => {
                        s.push_str(&format!("  - by fallback: {}: {how}{}\n", x.label, if x.note.is_empty() { String::new() } else { format!(" ({})", x.note) }));
                    }
                    Status::Failed(e) => {
                        s.push_str(&format!("  - FAILED: {}{}:\n", x.label, if x.command.is_empty() { String::new() } else { format!(" (`{}`)", x.command) }));
                        s.push_str(&indent_block(e));
                    }
                    Status::Skipped(why) => s.push_str(&format!("  - skipped: {}: {why}\n", x.label)),
                    Status::NotRun(_) => {}
                }
            }
            // Not run: once per reason, with every step it covers.
            let mut reasons: Vec<(&str, Vec<&str>)> = Vec::new();
            for x in &in_g {
                if let Status::NotRun(why) = &x.status {
                    match reasons.iter_mut().find(|(w, _)| *w == why.as_str()) {
                        Some((_, l)) => l.push(&x.label),
                        None => reasons.push((why, vec![&x.label])),
                    }
                }
            }
            for (why, labels) in reasons {
                let shown: Vec<&str> = labels.iter().take(40).copied().collect();
                let more = if labels.len() > shown.len() { format!("; and {} more (see conversion-report.json)", labels.len() - shown.len()) } else { String::new() };
                s.push_str(&format!("  - {} not run ({}): {}{more}\n", labels.len(), why.trim_start_matches("not run: "), shown.join("; ")));
            }
        }
        s.push('\n');
    }
    s.push_str(&timing_section(runs));
    s
}

fn timing_section(runs: &[DesignResult]) -> String {
    let mut s = String::from("## Where the time went\n\n");
    let all: Vec<(&DesignResult, &StepResult)> = runs.iter().flat_map(|r| r.steps.iter().map(move |x| (r, x))).collect();
    let mut by_cmd: BTreeMap<&str, (usize, f64, f64)> = BTreeMap::new();
    for (_, x) in &all {
        if x.calls == 0 {
            continue;
        }
        let key = if x.command.is_empty() { x.op.as_str() } else { x.command.as_str() };
        let e = by_cmd.entry(key).or_insert((0, 0.0, 0.0));
        e.0 += 1;
        e.1 += x.ms;
        e.2 = e.2.max(x.ms);
    }
    let mut rows: Vec<_> = by_cmd.into_iter().collect();
    rows.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1));
    s.push_str("| Command | Steps | Total s | Mean s | Slowest s |\n|---|---|---|---|---|\n");
    for (c, (n, total, max)) in rows {
        s.push_str(&format!("| `{c}` | {n} | {:.1} | {:.2} | {:.2} |\n", total / 1000.0, total / 1000.0 / n as f64, max / 1000.0));
    }
    let mut slow: Vec<&(&DesignResult, &StepResult)> = all.iter().filter(|(_, x)| x.calls > 0).collect();
    slow.sort_by(|a, b| b.1.ms.total_cmp(&a.1.ms));
    if !slow.is_empty() {
        s.push_str("\nSlowest steps:\n\n");
        for (r, x) in slow.into_iter().take(10) {
            s.push_str(&format!("- {:.1} s: {} ({}){}\n", x.ms / 1000.0, x.label, r.name, if x.batched { ", in a batch" } else { "" }));
        }
    }
    let batched = all.iter().filter(|(_, x)| x.batched).count();
    if batched > 0 {
        s.push_str(&format!("\n{batched} step(s) went to SolidCraft in batches.\n"));
    }
    s
}

pub fn builds_json(runs: &[DesignResult]) -> Json {
    Json::Arr(
        runs.iter()
            .map(|r| {
                obj! {
                    "design" => r.name.as_str(), "file" => r.file.as_str(), "saved" => r.saved, "ms" => r.ms.round(),
                    "stopped" => r.stopped.clone(), "session_lost" => r.session_lost.clone(),
                    "steps" => Json::Arr(r.steps.iter().map(|s| {
                        let mut j = obj!{"step" => s.label.as_str(), "group" => s.group.as_str(), "op" => s.op.as_str(), "command" => s.command.as_str(), "status" => s.status.name(), "detail" => s.status.detail(), "ms" => (s.ms * 10.0).round() / 10.0, "calls" => s.calls};
                        if !s.note.is_empty() { j.set("note", s.note.as_str()); }
                        if s.batched { j.set("batched", true); }
                        if !s.data.is_null() { j.set("data", s.data.clone()); }
                        j
                    }).collect()),
                }
            })
            .collect(),
    )
}

pub fn json(a: &Analysis, runs: &[DesignResult], events: &[String]) -> Json {
    let m = &a.model;
    obj! {
        "schema" => "lcw-report/2",
        "source" => m.source_file.as_str(),
        "sheets" => Json::Arr(a.sheets.iter().map(|(d, sp)| obj!{"part" => m.defs[*d].name.as_str(), "rule" => sp.rule.as_str(), "thickness" => sp.thickness, "bend_radius" => sp.bend_radius, "k_factor" => sp.k_factor, "k_assumed" => sp.k_assumed, "bends" => sp.bends.len()}).collect()),
        "joints" => Json::Arr(a.joints.joints.iter().map(|j| obj!{"name" => j.name.as_str(), "type" => j.kind.name(), "a" => name_of(a, &j.a), "b" => name_of(a, &j.b), "origin" => Json::from(j.origin.to_array()), "axis" => Json::from(j.z.to_array()), "basis" => j.basis(), "evidence" => j.evidence.clone()}).collect()),
        "evidence" => Json::Arr(a.joints.evidence.iter().map(|e| obj!{"name" => e.name.as_str(), "assembly" => e.assembly.as_str(), "kind" => e.kind.as_str(), "a" => name_of(a, &e.a), "b" => name_of(a, &e.b), "why_not" => e.why_not.as_str(), "outcome" => e.outcome.as_str()}).collect()),
        "patterns" => Json::Arr(a.patterns.iter().map(|p| obj!{"name" => p.name.as_str(), "kind" => describe_pattern(&p.found.kind), "members" => p.found.members.len()}).collect()),
        "notes" => a.notes.clone(),
        "events" => events.to_vec(),
        "builds" => builds_json(runs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(label: &str, group: &str, op: &str, status: Status) -> StepResult {
        StepResult { label: label.into(), group: group.into(), op: op.into(), status, command: format!("cmd.{op}"), ms: 1500.0, calls: 1, batched: false, note: String::new(), data: lcw_json::Json::Null }
    }

    #[test]
    fn the_build_section_reports_results_not_the_plan() {
        let run = DesignResult {
            name: "Widget frame".into(),
            file: "frame.sc".into(),
            saved: true,
            ms: 4000.0,
            stopped: Some("not run: step 2 (place Widget:1) failed, and the rest of this design needs it".into()),
            session_lost: None,
            steps: vec![
                step("place Widget:1", "placement", "place", Status::Failed("occurrence 7: not rigid\nsecond line of the error".into())),
                step("mate M1", "constraints", "constraint", Status::Fallback("joint".into())),
                step("limit Hinge", "limits", "auto_limits", Status::Skipped("limit not set, one end unknown: min unknown".into())),
                step("place Widget:2", "placement", "place", Status::NotRun("not run: step 2 (place Widget:1) failed, and the rest of this design needs it".into())),
                step("save", "save", "save", Status::Done),
            ],
        };
        let md = build_section(std::slice::from_ref(&run), &["SolidCraft was started again".into()]);
        assert!(md.contains("FAILED: place Widget:1 (`cmd.place`)"), "{md}");
        assert!(md.contains("second line of the error"), "the whole error text: {md}");
        assert!(md.contains("1 not run (step 2 (place Widget:1) failed, and the rest of this design needs it): place Widget:2"), "{md}");
        assert!(md.contains("by fallback: mate M1: joint"), "{md}");
        assert!(md.contains("skipped: limit Hinge: limit not set, one end unknown"), "{md}");
        assert!(md.contains("SolidCraft was started again"));
        assert!(md.contains("| `cmd.place` | 2 |") || md.contains("| `cmd.place` | 1 |"), "time per command: {md}");
        let summary = result_summary(&[run]);
        assert!(summary.contains("| placement | 2 | 0 | 0 | 0 | 1 | 1 |"), "{summary}");
        assert!(summary.contains("Joint limits: 1 planned, 0 set, 0 free"), "{summary}");
    }

    #[test]
    fn a_part_is_built_as_what_its_steps_achieved() {
        let mut r = DesignResult { name: "P".into(), file: "p.sc".into(), saved: true, ms: 0.0, stopped: None, session_lost: None, steps: vec![step("make P sheet metal", "sheet metal", "sheet_convert", Status::Failed("sew: the faces do not close".into()))] };
        assert_eq!(built_as(Some(&r), true), "solid (sheet metal conversion failed)");
        r.steps[0].status = Status::Done;
        assert_eq!(built_as(Some(&r), true), "sheet metal");
        r.saved = false;
        r.session_lost = Some("timeout".into());
        assert!(built_as(Some(&r), true).contains("session was lost"));
        assert_eq!(built_as(None, false), "not built");
    }
}
