//! `verification-report.md` and `verification.json`.

use lcw_json::{obj, Json};

use crate::judge::Tol;
use crate::verify::{Report, Status};

pub const SCHEMA: &str = "inv2sc-verification/1";

pub fn json(r: &Report, tol: &Tol) -> Json {
    obj! {
        "schema" => SCHEMA,
        "passed" => !r.failed(),
        "bundle" => r.bundle.as_str(),
        "built" => r.built.as_str(),
        "assembly" => r.assembly.as_str(),
        "solidcraft" => r.solidcraft.as_str(),
        "seconds" => r.seconds,
        "tolerances" => obj! {
            "position_mm" => tol.pos_mm, "position_rel_of_diagonal" => tol.pos_rel, "angle_deg" => tol.angle_deg,
            "inertia_rel" => tol.inertia_rel, "volume_rel_vs_step" => tol.volume_rel, "volume_rel_vs_inventor" => tol.bundle_volume_rel,
            "bbox_mm_vs_step" => tol.bbox_mm, "bbox_mm_vs_inventor" => tol.bundle_bbox_mm, "bbox_rel_vs_inventor" => tol.bundle_bbox_rel,
            "interference_fail_fraction" => tol.interference_fail_fraction, "interference_budget_s" => tol.interference_budget_s,
            "units" => "millimetres, degrees; SolidCraft reports mm, mm3 and g mm2; the bundle is mm",
        },
        "checks" => Json::Arr(r.checks.iter().map(|c| obj! {
            "id" => c.id as u32, "title" => c.title.as_str(), "status" => c.status.name(), "summary" => c.summary.as_str(),
            "details" => c.details.clone(), "data" => c.data.clone(),
        }).collect()),
        "limits" => r.limits.clone(),
    }
}

pub fn markdown(r: &Report, tol: &Tol) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Verification: {}\n\n", if r.failed() { "FAILED" } else { "passed" }));
    s.push_str(&format!("Bundle `{}`  \nBuilt assembly `{}`  \nSolidCraft {}  \nChecked in {:.0} s.\n\n", r.bundle, r.assembly, r.solidcraft, r.seconds));
    s.push_str("| # | Check | Result |\n|---|---|---|\n");
    for c in &r.checks {
        s.push_str(&format!("| {} | {} | **{}** {} |\n", c.id, c.title, c.status.name(), one_line(&c.summary)));
    }
    s.push_str(&format!(
        "\n## How it is checked\n\nSolidCraft is asked what it holds; the bundle says what Inventor had; the two are compared here. Expected world placements are the products of the bundle's occurrence matrices down each path (computed here, not taken from the converter). Units: the bundle is in millimetres and degrees, SolidCraft reports mm, mm3 and g mm2, so no unit conversion is applied; a systematic volume factor would be reported as a units problem.\n\nTolerances: position {} mm or {} of the part's diagonal, whichever is larger; frame angle {} deg; inertia per unit mass {}%; volume {}% against SolidCraft's own import of the STEP and {}% against Inventor's; box {} mm against the import, {} mm or {}% of the diagonal against Inventor's range box (which is not tight on curved parts); an interference is suspect from {}% of the smaller body.\n\n",
        tol.pos_mm, tol.pos_rel, tol.angle_deg, tol.inertia_rel * 100.0, tol.volume_rel * 100.0, tol.bundle_volume_rel * 100.0, tol.bbox_mm, tol.bundle_bbox_mm, tol.bundle_bbox_rel * 100.0, tol.interference_fail_fraction * 100.0
    ));
    for c in &r.checks {
        s.push_str(&format!("## {}. {}: {}\n\n{}\n\n", c.id, c.title, c.status.name(), c.summary));
        if !c.details.is_empty() {
            let shown = if c.id == 5 { 40 } else { 400 };
            for d in c.details.iter().take(shown) {
                s.push_str(&format!("- {d}\n"));
            }
            if c.details.len() > shown {
                s.push_str(&format!("- ... and {} more (all in verification.json)\n", c.details.len() - shown));
            }
            s.push('\n');
        }
        if c.id == 2 {
            if let Some(Json::Arr(rows)) = c.data.get("placements") {
                let off: Vec<&Json> = rows.iter().filter(|r| r.str_of("status") == Some("equivalent")).collect();
                if !off.is_empty() {
                    s.push_str("Same geometry, another frame (not an error: a symmetric part, or a part merged into an identical one):\n\n");
                    for r in off.iter().take(40) {
                        s.push_str(&format!("- {} ({}): frame {} mm, {} deg\n", r.str_of("built").unwrap_or(""), r.str_of("part").unwrap_or(""), num(r.f64_of("frame_mm")), num(r.f64_of("frame_deg"))));
                    }
                    s.push('\n');
                }
            }
        }
    }
    s.push_str("## What could not be checked\n\n");
    for l in &r.limits {
        s.push_str(&format!("- {l}\n"));
    }
    s.push('\n');
    let _ = Status::Pass;
    s
}

fn one_line(t: &str) -> String {
    t.replace('|', "/").replace('\n', " ")
}

fn num(x: Option<f64>) -> String {
    x.map_or("?".into(), |x| format!("{x:.3}"))
}
