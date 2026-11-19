//! `cargo run --example analyze -- FILE.stp|BUNDLE`: print what the analysis finds.
use lcw_analyze::{analyze, model::Model, Options};

fn main() {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: analyze FILE.stp|BUNDLE_DIR");
        std::process::exit(2);
    };
    let path = std::path::Path::new(&arg);
    let t = std::time::Instant::now();
    let model = if path.is_dir() || path.extension().is_some_and(|e| e == "json") {
        Model::from_bundle(&lcw_bundle::Bundle::read(path).expect("bundle")).expect("model")
    } else {
        Model::from_step(path).expect("step")
    };
    let a = analyze(model, &Options::default());
    let m = &a.model;
    let name = |p: &[usize]| {
        let ids = m.path_ids(p);
        ids.last().cloned().unwrap_or_default()
    };
    println!("{} defs, {} instances, {:?}", m.defs.len(), a.instances.len(), t.elapsed());
    println!("merged copies: {:?}", a.merged.iter().map(|(p, c)| (m.defs[*p].name.clone(), m.defs[*c].name.clone())).collect::<Vec<_>>());
    println!("unmerged copies: {:?}", a.copies.iter().map(|c| (m.defs[c.part].name.clone(), m.defs[c.of].name.clone())).collect::<Vec<_>>());
    println!("mirrors: {:?}", a.mirrors.iter().map(|c| (m.defs[c.part].name.clone(), m.defs[c.of].name.clone())).collect::<Vec<_>>());
    println!("sheets:");
    for (d, s) in &a.sheets {
        println!("  {:50} t={:.2} r={:.2} bends={} {:?}", m.defs[*d].name, s.thickness, s.bend_radius, s.bends.len(), s.source);
    }
    println!("patterns:");
    for p in &a.patterns {
        println!("  {} in {}: {:?}", p.name, m.defs[p.assembly].name, p.found.kind);
    }
    println!("symmetry planes:");
    for s in &a.symmetry {
        println!("  point {:?} normal {:?} pairs {}", s.point.round(0.01), s.normal.round(0.001), s.pairs.len());
    }
    println!("ground: {:?}", a.joints.ground.iter().map(|p| name(p)).collect::<Vec<_>>());
    println!("joints:");
    for j in &a.joints.joints {
        println!("  {:9} {:45} -> {:45} at {:?} z {:?}", j.kind.name(), name(&j.a), name(&j.b), j.origin.round(0.01), j.z.round(0.001));
    }
    println!("rigid groups: {}", a.joints.rigid_groups.len());
    for g in &a.joints.rigid_groups {
        println!("  [{}] {}", name(&g.members[0]), g.members[1..].iter().map(|p| name(p)).collect::<Vec<_>>().join(", "));
    }
    for n in &a.notes {
        println!("note: {n}");
    }
}
