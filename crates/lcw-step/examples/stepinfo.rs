//! Print a STEP file's product tree: `cargo run --example stepinfo -- file.stp`.
fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: stepinfo FILE.stp");
        std::process::exit(2);
    };
    let t = std::time::Instant::now();
    match lcw_step::StepModel::read(std::path::Path::new(&path)) {
        Ok(m) => {
            println!("{} products, {} roots, {} warnings, {:?}", m.products.len(), m.roots.len(), m.warnings.len(), t.elapsed());
            for w in m.warnings.iter().take(10) {
                println!("warning: {w}");
            }
            for &r in &m.roots {
                for (p, w, path) in m.instances(r).iter().take(400) {
                    let pr = &m.products[*p];
                    let faces: usize = pr.solids.iter().map(|s| s.faces.len()).sum();
                    let o = w.origin();
                    println!("{}{} [{}] solids={} faces={} at ({:.1},{:.1},{:.1})", "  ".repeat(path.len()), pr.name, path.last().map_or("", |s| s), pr.solids.len(), faces, o.x, o.y, o.z);
                }
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
