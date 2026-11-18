//! The whole chain on the test assemblies, against a recording SolidCraft: every step's
//! references resolve, every design is saved, and the assembly gets its placements, ground,
//! joints and pattern.

use std::collections::BTreeMap;
use std::path::Path;

use lcw_analyze::{analyze, model::Model, Options};
use lcw_solidcraft::emit::{assembly_steps, part_steps, Options as EmitOptions};
use lcw_solidcraft::mcp::Recorder;
use lcw_solidcraft::profile::Profile;
use lcw_solidcraft::run::{Runner, Status};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../lcw-analyze/tests").join(name)
}

#[test]
fn a_bracket_rack_builds_with_every_reference_resolved() {
    let model = Model::from_step(&fixture("bracket_rack.stp")).expect("model");
    let a = analyze(model, &Options::default());
    assert_eq!(a.sheets.len(), 2, "the bracket and the plate are sheet metal");
    assert_eq!(a.patterns.len(), 1, "four brackets in a row");
    let eo = EmitOptions { out_dir: "/out".into(), ..EmitOptions::default() };
    let mut files = BTreeMap::new();
    let mut parts = Vec::new();
    for (i, d) in a.model.defs.iter().enumerate().filter(|(_, d)| !d.assembly) {
        let file = format!("/out/parts/{}.sc", d.key);
        parts.push(part_steps(&a, i, &format!("/out/parts/{}.stp", d.key), &file, &eo));
        files.insert(i, file);
    }
    let asm = assembly_steps(&a, &files, "/out/rack.sc", &eo);
    let profile = Profile::builtin();
    let mut rec = Recorder::new();
    let mut runner = Runner::new(&mut rec, &profile);
    for d in parts.iter().chain(std::iter::once(&asm)) {
        let r = runner.run(d);
        assert!(r.saved, "{} not saved", d.name);
        for s in &r.steps {
            if let Status::Skipped(why) | Status::Failed(why) = &s.status {
                // Limits are skipped when nothing stops a joint; nothing else may be.
                assert!(why.contains("turns freely") || why.contains("nothing stops"), "{}: {} — {why}", d.name, s.label);
            }
        }
    }
    let cmds: Vec<String> = rec.calls.iter().filter_map(|(_, a)| a.str_of("command").map(str::to_string)).collect();
    assert_eq!(cmds.iter().filter(|c| *c == "sheet.convert").count(), 2);
    assert_eq!(cmds.iter().filter(|c| *c == "component.insert_design").count(), 2, "one linked component per part");
    assert_eq!(cmds.iter().filter(|c| *c == "occurrence.copy").count(), 4, "the other placements copy it");
    assert!(cmds.iter().any(|c| c == "parameters.change"), "the pattern spacing is a parameter");
    assert!(cmds.iter().any(|c| c == "occurrence.ground"));
}
