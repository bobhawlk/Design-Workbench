use lcw_step::{StepModel, Surface};
fn main() {
    let m = StepModel::read(std::path::Path::new(&std::env::args().nth(1).unwrap())).unwrap();
    for p in &m.products {
        for s in &p.solids {
            println!("{} faces {}", p.name, s.faces.len());
            for f in &s.faces {
                let k = match &f.surface { Surface::Plane{..} => "plane".to_string(), Surface::Cylinder{radius,..} => format!("cyl r{radius}"), o => format!("{o:?}") };
                println!("  {k} same={} n={:?} loops={:?} interior={:?}", f.same_sense, f.plane_normal(), f.loops.iter().map(|l| l.len()).collect::<Vec<_>>(), lcw_analyze::features::interior_point(f));
            }
            println!("plan: {:?}", lcw_analyze::sheet::plan(std::slice::from_ref(s), None, None, &Default::default()).map(|p| (p.thickness, p.bends.clone())));
        }
    }
}
