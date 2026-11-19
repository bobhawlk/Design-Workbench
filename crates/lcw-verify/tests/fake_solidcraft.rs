//! `inv2sc verify` against a scripted fake SolidCraft that answers the way the real one does
//! (the shapes were taken from SolidCraft 0.44 answers; every part and number here is invented).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use lcw_bundle::{Bundle, DocKind, Document, Mass, Occurrence, SCHEMA};
use lcw_geom::{v3, Bbox, M4, V3};
use lcw_json::{obj, Json};
use lcw_solidcraft::mcp::{CallError, Transport};
use lcw_solidcraft::profile::Profile;
use lcw_verify::session::Session;
use lcw_verify::{run, Inputs, Status, Tol};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("inv2sc-verify-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// What a built assembly is made of.
#[derive(Clone)]
enum Node {
    /// An occurrence name, a part name and its placement.
    Part(&'static str, &'static str, M4),
    Asm(&'static str, &'static str, M4, Vec<Node>),
}

/// An invented block 10 x 20 x 30 mm (scaled per part) with its corner at the part's origin.
fn size_of(part: &str) -> (f64, f64, f64) {
    match part {
        "Plate" => (40.0, 20.0, 4.0),
        "Pin" => (6.0, 6.0, 30.0),
        _ => (10.0, 20.0, 30.0),
    }
}

#[derive(Clone, Default)]
struct Design {
    #[allow(dead_code)]
    name: String,
    /// (id, name, children occurrence ids, block)
    comps: Vec<(u64, String, Vec<u64>, Option<(f64, f64, f64)>)>,
    /// (id, name, component placed, parent component, local placement)
    occs: Vec<(u64, String, u64, u64, M4)>,
}

struct Builder {
    d: Design,
    next: u64,
    by_part: BTreeMap<String, u64>,
    scale: f64,
}

impl Builder {
    fn comp_for(&mut self, name: &str, block: Option<(f64, f64, f64)>) -> u64 {
        if let Some(c) = self.by_part.get(name) {
            return *c;
        }
        let id = self.next;
        self.next += 1;
        let b = block.map(|(a, b, c)| (a * self.scale, b * self.scale, c * self.scale));
        self.d.comps.push((id, name.to_string(), vec![], b));
        self.by_part.insert(name.to_string(), id);
        id
    }

    fn add(&mut self, parent: u64, nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Part(occ, part, m) => {
                    let c = self.comp_for(part, Some(size_of(part)));
                    let oid = self.next;
                    self.next += 1;
                    self.d.occs.push((oid, occ.to_string(), c, parent, *m));
                    self.d.comps.iter_mut().find(|c| c.0 == parent).unwrap().2.push(oid);
                }
                Node::Asm(occ, asm, m, kids) => {
                    let fresh = !self.by_part.contains_key(*asm);
                    let c = self.comp_for(asm, None);
                    let oid = self.next;
                    self.next += 1;
                    self.d.occs.push((oid, occ.to_string(), c, parent, *m));
                    self.d.comps.iter_mut().find(|c| c.0 == parent).unwrap().2.push(oid);
                    if fresh {
                        self.add(c, kids);
                    }
                }
            }
        }
    }
}

fn build_design(name: &str, nodes: &[Node], scale: f64) -> Design {
    let mut b = Builder { d: Design { name: name.into(), ..Design::default() }, next: 1, by_part: BTreeMap::new(), scale };
    b.d.comps.push((0, name.to_string(), vec![], None));
    b.add(0, nodes);
    b.d
}

fn row_vector(m: &M4) -> Json {
    Json::Arr((0..4).map(|c| Json::Arr((0..4).map(|r| Json::from(m.0[r][c])).collect())).collect())
}

/// A scripted SolidCraft: the assembly is `asm`, a part's design or STEP opens as one block.
struct Fake {
    asm: Design,
    current: Design,
    /// Scale applied to the built part designs' blocks (a units fault when not 1).
    part_scale: BTreeMap<String, f64>,
    clash: Json,
    files: BTreeMap<String, String>,
}

impl Fake {
    fn new(asm: Design, clash: Json) -> Fake {
        let mut f = Fake { asm: asm.clone(), current: asm, part_scale: BTreeMap::new(), clash, files: BTreeMap::new() };
        f.files.insert("Plate".into(), "/built/parts/Plate.sc".into());
        f.files.insert("Pin".into(), "/built/parts/Pin.sc".into());
        f.files.insert("Block".into(), "/built/parts/Block.sc".into());
        f
    }

    fn world(&self, path: &[u64]) -> (M4, u64) {
        let mut w = M4::IDENTITY;
        let mut comp = 0;
        for id in path {
            let o = self.current.occs.iter().find(|o| o.0 == *id).unwrap();
            w = w * o.4;
            comp = o.2;
        }
        (w, comp)
    }

    fn props(&self, path: &[u64]) -> Json {
        let (w, comp) = self.world(path);
        let (a, b, c) = self.current.comps.iter().find(|c| c.0 == comp).and_then(|c| c.3).unwrap_or((1.0, 1.0, 1.0));
        let (vol, m) = (a * b * c, a * b * c * 1e-3);
        let i = [[m / 12.0 * (b * b + c * c), 0.0, 0.0], [0.0, m / 12.0 * (a * a + c * c), 0.0], [0.0, 0.0, m / 12.0 * (a * a + b * b)]];
        let iw = lcw_verify::judge::rotate_inertia(&w, &i);
        let bb = lcw_verify::judge::transformed_box(&Bbox { min: V3::ZERO, max: v3(a, b, c) }, &w);
        let com = w.point(v3(a / 2.0, b / 2.0, c / 2.0));
        obj! {
            "bodies" => vec![obj!{"closed" => true, "name" => "Solid"}],
            "total" => obj!{
                "volume_mm3" => vol, "mass_g" => m, "center_of_mass" => com.to_array(),
                "inertia_g_mm2" => Json::Arr(iw.iter().map(|r| Json::from(r.to_vec())).collect()),
                "bbox" => obj!{"min" => bb.min.to_array(), "max" => bb.max.to_array()},
            },
        }
    }

    fn components(&self) -> Json {
        let comps: Vec<Json> = self
            .current
            .comps
            .iter()
            .map(|(id, name, children, _)| {
                let occs: Vec<Json> = self.current.occs.iter().filter(|o| o.2 == *id).map(|o| obj! {"id" => o.0 as f64, "name" => o.1.as_str(), "parent" => o.3 as f64, "transform" => row_vector(&o.4)}).collect();
                obj! {"id" => *id as f64, "name" => name.as_str(), "children" => children.iter().map(|c| *c as f64).collect::<Vec<_>>(), "occurrences" => Json::Arr(occs)}
            })
            .collect();
        obj! {"components" => Json::Arr(comps)}
    }
}

impl Transport for Fake {
    fn call(&mut self, tool: &str, args: Json) -> Result<Json, String> {
        self.call_within(tool, args, Duration::from_secs(60)).map_err(|e| e.message().to_string())
    }

    fn call_within(&mut self, tool: &str, args: Json, _t: Duration) -> Result<Json, CallError> {
        let fail = |m: String| Err(CallError::Failed(m));
        match tool {
            "version" => Ok(obj! {"version" => "0.44.test", "mode" => "headless"}),
            "open" => {
                let path = args.str_of("path").unwrap_or("").to_string();
                let stem = path.rsplit('/').next().unwrap_or("").rsplit_once('.').map(|x| x.0).unwrap_or("").to_string();
                if path.ends_with("asm.sc") {
                    self.current = self.asm.clone();
                    return Ok(obj! {"name" => "asm"});
                }
                if let Some(part) = stem.strip_prefix("step-") {
                    self.current = build_design(part, &[Node::Part("p:1", Box::leak(part.to_string().into_boxed_str()), M4::IDENTITY)], 1.0);
                    return Ok(obj! {"name" => format!("step-{part}")});
                }
                if self.files.values().any(|f| f.ends_with(&format!("{stem}.sc"))) {
                    let s = self.part_scale.get(&stem).copied().unwrap_or(1.0);
                    self.current = build_design(&stem, &[Node::Part("p:1", Box::leak(stem.clone().into_boxed_str()), M4::IDENTITY)], s);
                    return Ok(obj! {"name" => stem.as_str()});
                }
                fail(format!("{path}: no such design"))
            }
            "measure" => {
                // One body per placed instance, in the world, as SolidCraft lists them.
                fn walk(f: &Fake, comp: u64, path: &mut Vec<u64>, out: &mut Vec<Json>) {
                    let kids = f.current.comps.iter().find(|c| c.0 == comp).map(|c| c.2.clone()).unwrap_or_default();
                    for oid in kids {
                        path.push(oid);
                        let o = f.current.occs.iter().find(|o| o.0 == oid).unwrap();
                        let c = f.current.comps.iter().find(|c| c.0 == o.2).unwrap();
                        if c.3.is_some() {
                            let p = f.props(path);
                            let t = p.get("total").unwrap();
                            out.push(obj! {"name" => format!("{} ({})", c.1, o.1), "closed" => true, "volume_mm3" => t.f64_of("volume_mm3"), "bbox" => t.get("bbox").cloned()});
                        } else {
                            walk(f, o.2, path, out);
                        }
                        path.pop();
                    }
                }
                let mut bodies = Vec::new();
                walk(self, 0, &mut Vec::new(), &mut bodies);
                Ok(obj! {"bodies" => Json::Arr(bodies)})
            }
            "execute" => match args.str_of("command").unwrap_or("") {
                "component.list" => Ok(self.components()),
                "component.links" => {
                    let links: Vec<Json> = self.current.comps.iter().filter(|c| c.3.is_some()).map(|c| obj! {"component" => c.0 as f64, "file" => self.files.get(&c.1).cloned().unwrap_or_default(), "found" => true}).collect();
                    Ok(obj! {"links" => Json::Arr(links)})
                }
                "occurrence.properties" => {
                    let path: Vec<u64> = args.pointer("params/path").map(|p| p.as_array().unwrap_or(&[]).iter().filter_map(Json::as_u64).collect()).unwrap_or_default();
                    Ok(self.props(&path))
                }
                "inspect.interference" => Ok(self.clash.clone()),
                other => fail(format!("unscripted command {other}")),
            },
            other => fail(format!("unscripted tool {other}")),
        }
    }
}

fn doc(id: &str, name: &str, kind: DocKind) -> Document {
    Document {
        id: id.into(),
        name: name.into(),
        kind,
        step: (kind == DocKind::Part).then(|| format!("parts/step-{name}.stp")),
        mass: if kind == DocKind::Part {
            let (a, b, c) = size_of(name);
            Mass { volume_mm3: Some(a * b * c), bbox_min: Some(V3::ZERO), bbox_max: Some(v3(a, b, c)), ..Mass::default() }
        } else {
            Mass::default()
        },
        ..Document::default()
    }
}

fn occ(id: &str, doc: &str, m: M4) -> Occurrence {
    Occurrence { id: id.into(), name: id.into(), document: doc.into(), transform: m, visible: true, ..Occurrence::default() }
}

fn t(x: f64, y: f64, z: f64) -> M4 {
    M4::translation(v3(x, y, z))
}

/// The invented Inventor assembly: a plate, a block, and a "Pair" sub-assembly (a plate and a
/// pin) placed twice.
fn bundle() -> Bundle {
    let mut top = doc("top", "Top", DocKind::Assembly);
    top.occurrences = vec![
        occ("Plate:1", "plate", t(0.0, 0.0, 0.0)),
        occ("Block:1", "block", M4::rotation(v3(100.0, 0.0, 0.0), V3::Z, 90f64.to_radians())),
        occ("Pair:1", "pair", t(0.0, 100.0, 0.0)),
        occ("Pair:2", "pair", M4::rotation(v3(0.0, 200.0, 0.0), V3::Z, 90f64.to_radians())),
    ];
    let mut pair = doc("pair", "Pair", DocKind::Assembly);
    pair.occurrences = vec![occ("Plate:1", "plate", t(1.0, 2.0, 3.0)), occ("Pin:1", "pin", t(10.0, 5.0, 4.0))];
    Bundle { schema: SCHEMA.into(), root: "top".into(), documents: vec![top, pair, doc("plate", "Plate", DocKind::Part), doc("block", "Block", DocKind::Part), doc("pin", "Pin", DocKind::Part)], ..Bundle::default() }
}

/// What a correct conversion of that bundle builds.
fn good_nodes() -> Vec<Node> {
    vec![
        Node::Part("Plate:1", "Plate", t(0.0, 0.0, 0.0)),
        Node::Part("Block:1", "Block", M4::rotation(v3(100.0, 0.0, 0.0), V3::Z, 90f64.to_radians())),
        Node::Asm("Pair:1", "Pair", t(0.0, 100.0, 0.0), vec![Node::Part("Plate:1", "Plate", t(1.0, 2.0, 3.0)), Node::Part("Pin:1", "Pin", t(10.0, 5.0, 4.0))]),
        Node::Asm("Pair:2", "Pair", M4::rotation(v3(0.0, 200.0, 0.0), V3::Z, 90f64.to_radians()), vec![]),
    ]
}

fn verify(name: &str, nodes: &[Node], scale: BTreeMap<String, f64>, clash: Json, edit: impl Fn(&mut Bundle)) -> lcw_verify::Report {
    let dir = tmp(name);
    let mut b = bundle();
    edit(&mut b);
    b.write(&dir).unwrap();
    let b = Bundle::read(&dir).unwrap();
    let mut fake = Fake::new(build_design("asm", nodes, 1.0), clash);
    fake.part_scale = scale;
    let profile = Profile::builtin();
    let mut s = Session::new(&mut fake, &profile, Duration::from_secs(60));
    let log = |_: &str| {};
    run(&mut s, &Inputs { bundle: &b, built: &PathBuf::from("/built"), assembly: &PathBuf::from("/built/asm.sc"), tol: Tol::default(), log: &log }).unwrap()
}

fn no_clash() -> Json {
    obj! {"interferences" => Json::Arr(vec![]), "unchecked" => Json::Arr(vec![]), "budget_exceeded" => false}
}

#[test]
fn a_correct_build_passes_every_check() {
    let r = verify("good", &good_nodes(), BTreeMap::new(), no_clash(), |_| {});
    for c in &r.checks {
        assert_ne!(c.status, Status::Fail, "check {} failed: {}\n{:#?}", c.id, c.summary, c.details);
    }
    assert!(!r.failed());
    let c2 = r.check(2).unwrap();
    assert!(c2.summary.contains("6 of 6 placed parts are present once"), "{}", c2.summary);
    assert!(c2.summary.contains("sub-assemblies 2 of 2"), "{}", c2.summary);
    // The second copy of the sub-assembly lists no children in SolidCraft: its parts are still
    // found, placed through the copy's own matrix.
    assert_eq!(c2.data.f64_of("misplaced"), Some(0.0));
    assert!(r.summary_line().starts_with("verify: PASS"), "{}", r.summary_line());
}

#[test]
fn a_shifted_missing_extra_and_renamed_part_are_each_reported_with_numbers() {
    let mut nodes = good_nodes();
    // The block is 7 mm off in x, one plate of the first Pair is gone, a stray pin is added at
    // the top and the top plate has another name.
    nodes[1] = Node::Part("Block:1", "Block", M4::rotation(v3(100.0, 0.0, 0.0), V3::Z, 90f64.to_radians()) * t(7.0, 0.0, 0.0));
    nodes[2] = Node::Asm("Pair:1", "Pair", t(0.0, 100.0, 0.0), vec![Node::Part("Pin:1", "Pin", t(10.0, 5.0, 4.0))]);
    nodes.push(Node::Part("Pin:9", "Pin", t(500.0, 0.0, 0.0)));
    nodes[0] = Node::Part("Base plate:1", "Plate", t(0.0, 0.0, 0.0));
    let r = verify("faults", &nodes, BTreeMap::new(), no_clash(), |_| {});
    assert!(r.failed());
    let c2 = r.check(2).unwrap();
    assert_eq!(c2.status, Status::Fail);
    let text = c2.details.join("\n");
    assert!(text.contains("MISPLACED Block:1 (Block)") && text.contains("7.000 mm"), "{text}");
    assert!(text.contains("MISSING Pair:1 > Plate:1") || text.contains("MISSING Pair:2 > Plate:1") || text.contains("MISSING"), "{text}");
    assert!(text.contains("EXTRA Pin:9"), "{text}");
    assert_eq!(c2.data.f64_of("misplaced"), Some(1.0));
    let c3 = r.check(3).unwrap();
    assert_eq!(c3.status, Status::Fail);
    assert!(c3.details.join("\n").contains("`Plate:1` is `Base plate:1`"), "{:?}", c3.details);
    let c4 = r.check(4).unwrap();
    assert_eq!(c4.status, Status::Fail, "{}", c4.summary);
}

#[test]
fn a_part_design_with_the_wrong_volume_is_named() {
    let mut scale = BTreeMap::new();
    scale.insert("Pin".to_string(), 1.1);
    let r = verify("volume", &good_nodes(), scale, no_clash(), |_| {});
    let c1 = r.check(1).unwrap();
    assert_eq!(c1.status, Status::Fail);
    let text = c1.details.join("\n");
    assert!(text.contains("Pin: volume") && text.contains("against Inventor's"), "{text}");
    assert!(!text.contains("Plate:"), "only the Pin is wrong: {text}");
}

#[test]
fn every_volume_out_by_the_same_factor_is_called_a_units_problem() {
    let mut scale = BTreeMap::new();
    for p in ["Plate", "Pin", "Block"] {
        scale.insert(p.to_string(), 25.4);
    }
    let r = verify("units", &good_nodes(), scale, no_clash(), |_| {});
    let c1 = r.check(1).unwrap();
    assert_eq!(c1.status, Status::Fail);
    assert!(c1.details[0].contains("systematic volume factor") && c1.details[0].contains("inches read as millimetres"), "{}", c1.details[0]);
}

#[test]
fn a_bundle_without_inventor_volumes_falls_back_to_the_step_and_says_so() {
    let r = verify("nomass", &good_nodes(), BTreeMap::new(), no_clash(), |b| {
        for d in &mut b.documents {
            d.mass = Mass::default();
        }
    });
    let c1 = r.check(1).unwrap();
    assert_eq!(c1.status, Status::Pass, "{:?}", c1.details);
    assert!(c1.summary.contains("no Inventor volume or box for any part"), "{}", c1.summary);
    assert!(r.limits.iter().any(|l| l.contains("0 of 3 parts")), "{:?}", r.limits);
}

#[test]
fn interference_is_information_unless_an_overlap_is_large() {
    let small = obj! {
        "interferences" => vec![obj!{"a" => "Plate (Plate:1)", "b" => "Pin (Pin:1)", "volume_mm3" => 12.0, "a_part" => obj!{"path" => vec!["Plate:1"]}, "b_part" => obj!{"path" => vec!["Pair:1", "Pin:1"]}}],
        "unchecked" => Json::Arr(vec![]), "budget_exceeded" => false,
    };
    let r = verify("clash-small", &good_nodes(), BTreeMap::new(), small, |_| {});
    let c5 = r.check(5).unwrap();
    assert_eq!(c5.status, Status::Info);
    assert!(c5.summary.contains("1 interfering pair"), "{}", c5.summary);

    // The same overlap between two parts that are where Inventor has them is in the source model.
    let large = |a: &str, b: &str| {
        obj! {
            "interferences" => vec![obj!{"a" => "Plate (Plate:1)", "b" => "Pin (Pin:1)", "volume_mm3" => 1000.0, "a_part" => obj!{"path" => vec![a]}, "b_part" => obj!{"path" => vec!["Pair:1", b]}}],
            "unchecked" => Json::Arr(vec![]), "budget_exceeded" => false,
        }
    };
    let r = verify("clash-source", &good_nodes(), BTreeMap::new(), large("Plate:1", "Pin:1"), |_| {});
    let c5 = r.check(5).unwrap();
    assert_eq!(c5.status, Status::Info, "{}", c5.summary);
    assert!(c5.details[0].contains("in the Inventor model too"), "{:?}", c5.details);

    // With one of them misplaced it is suspect: a misplacement.
    let mut nodes = good_nodes();
    nodes[0] = Node::Part("Plate:1", "Plate", t(30.0, 0.0, 0.0));
    let r = verify("clash-large", &nodes, BTreeMap::new(), large("Plate:1", "Pin:1"), |_| {});
    let c5 = r.check(5).unwrap();
    assert_eq!(c5.status, Status::Fail, "{}", c5.summary);
    assert!(c5.details[0].starts_with("SUSPECT"), "{:?}", c5.details);
}

#[test]
fn a_part_in_the_wrong_sub_assembly_is_reported_with_where_it_is_and_where_it_belongs() {
    // The pin is built at the top level instead of inside Pair, at the place Inventor has it.
    let mut nodes = good_nodes();
    nodes[2] = Node::Asm("Pair:1", "Pair", t(0.0, 100.0, 0.0), vec![Node::Part("Plate:1", "Plate", t(1.0, 2.0, 3.0))]);
    nodes.push(Node::Part("Pin:1", "Pin", t(10.0, 105.0, 4.0)));
    let r = verify("parent", &nodes, BTreeMap::new(), no_clash(), |_| {});
    let c2 = r.check(2).unwrap();
    assert_eq!(c2.status, Status::Fail);
    let text = c2.details.join("\n");
    assert!(text.contains("STRUCTURE Pin:1: it sits at the top level but Inventor has it in `Pair`"), "{text}");
    assert_eq!(c2.data.f64_of("wrong_parent"), Some(1.0));
}

#[test]
fn the_reports_are_written_and_machine_readable() {
    let r = verify("reports", &good_nodes(), BTreeMap::new(), no_clash(), |_| {});
    let j = lcw_verify::report::json(&r, &Tol::default());
    let parsed = Json::parse(&j.to_pretty()).unwrap();
    assert_eq!(parsed.str_of("schema"), Some("inv2sc-verification/1"));
    assert_eq!(parsed.bool_of("passed"), Some(true));
    assert_eq!(parsed.arr_of("checks").len(), 5);
    let md = lcw_verify::report::markdown(&r, &Tol::default());
    assert!(md.starts_with("# Verification: passed") && md.contains("## 2."), "{md}");
}
