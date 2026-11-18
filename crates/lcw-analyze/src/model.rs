//! One assembly model, whichever way it came in: an Inventor bundle or a plain STEP file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lcw_bundle::{Bundle, Constraint, DocKind, Joint, Mass, Pattern, SheetMetal};
use lcw_geom::M4;
use lcw_step::{Solid, StepFile};

/// A part or assembly definition (one Inventor document, one STEP product).
#[derive(Debug, Clone, Default)]
pub struct Def {
    /// Unique, file-name-safe key (also the SolidCraft component name stem).
    pub key: String,
    pub name: String,
    pub assembly: bool,
    /// The bundle document id, when the model came from a bundle.
    pub doc: Option<String>,
    /// Where this part's own STEP is (bundle part STEP), or None when it must be extracted.
    pub step: Option<PathBuf>,
    /// The PRODUCT_DEFINITION of a part in the source STEP (for extraction).
    pub step_pd: Option<u64>,
    /// Part geometry in the part's frame.
    pub solids: Vec<Solid>,
    pub sheet_metal: Option<SheetMetal>,
    pub mass: Mass,
    pub properties: BTreeMap<String, String>,
    pub part_number: Option<String>,
    pub material: Option<String>,
    pub occurrences: Vec<Occ>,
    pub constraints: Vec<Constraint>,
    pub joints: Vec<Joint>,
    pub patterns: Vec<Pattern>,
    /// Derived from another part (bundle): (source document, mirrored).
    pub derived: Option<(String, bool)>,
}

/// A placement of a definition inside an assembly definition.
#[derive(Debug, Clone, Default)]
pub struct Occ {
    pub id: String,
    pub name: String,
    pub def: usize,
    pub transform: M4,
    pub grounded: bool,
    pub suppressed: bool,
    pub visible: bool,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub defs: Vec<Def>,
    pub root: usize,
    pub source_app: String,
    pub source_file: String,
    pub warnings: Vec<String>,
    /// The source STEP (STEP-only input), kept to extract parts.
    pub step_path: Option<PathBuf>,
}

/// One placed part (or assembly) in the whole tree.
#[derive(Debug, Clone)]
pub struct Inst {
    pub def: usize,
    /// Occurrence indices from the root down (each into its parent def's occurrences).
    pub path: Vec<usize>,
    pub world: M4,
}

/// One line of text: runs of whitespace (STEP names often hold line breaks) become one space.
pub fn clean_name(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Model {
    /// Every placement below the root (the root itself excluded), suppressed ones left out.
    pub fn instances(&self) -> Vec<Inst> {
        let mut out = Vec::new();
        let mut stack = vec![(self.root, Vec::new(), M4::IDENTITY)];
        while let Some((def, path, world)) = stack.pop() {
            if path.len() > 32 {
                continue;
            }
            for (i, o) in self.defs[def].occurrences.iter().enumerate().rev() {
                if o.suppressed {
                    continue;
                }
                let mut p = path.clone();
                p.push(i);
                let w = world * o.transform;
                out.push(Inst { def: o.def, path: p.clone(), world: w });
                if self.defs[o.def].assembly {
                    stack.push((o.def, p, w));
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// The occurrence ids along a path.
    pub fn path_ids(&self, path: &[usize]) -> Vec<String> {
        let mut def = self.root;
        let mut out = Vec::new();
        for &i in path {
            let Some(o) = self.defs[def].occurrences.get(i) else { break };
            out.push(o.id.clone());
            def = o.def;
        }
        out
    }

    /// Find a path of occurrence indices from `asm` by ids.
    pub fn resolve(&self, asm: usize, ids: &[String]) -> Option<Vec<usize>> {
        let mut def = asm;
        let mut out = Vec::new();
        for id in ids {
            let i = self.defs[def].occurrences.iter().position(|o| &o.id == id || &o.name == id)?;
            out.push(i);
            def = self.defs[def].occurrences[i].def;
        }
        Some(out)
    }

    pub fn from_step(path: &Path) -> Result<Model, String> {
        let file = StepFile::read(path).map_err(|e| e.to_string())?;
        let step = file.model().map_err(|e| e.to_string())?;
        let mut m = Model {
            source_app: "STEP".into(),
            source_file: path.display().to_string(),
            warnings: step.warnings.clone(),
            step_path: Some(path.to_path_buf()),
            ..Default::default()
        };
        let mut keys = KeyMaker::default();
        // A product with children is an assembly; one with solids as well gets its bodies as an
        // extra part placed where it is.
        let mut index = vec![usize::MAX; step.products.len()];
        let mut step = step;
        for p in &mut step.products {
            p.name = clean_name(&p.name);
            p.id = clean_name(&p.id);
            for c in &mut p.children {
                c.name = clean_name(&c.name);
            }
        }
        for (i, p) in step.products.iter().enumerate() {
            let assembly = !p.children.is_empty();
            index[i] = m.defs.len();
            m.defs.push(Def {
                key: keys.make(&p.name),
                name: p.name.clone(),
                assembly,
                step_pd: (!assembly).then_some(p.pd),
                solids: if assembly { Vec::new() } else { p.solids.clone() },
                part_number: (!p.id.is_empty() && p.id != p.name).then(|| p.id.clone()),
                ..Default::default()
            });
        }
        for (i, p) in step.products.iter().enumerate() {
            if p.children.is_empty() {
                continue;
            }
            let di = index[i];
            let mut seen: BTreeMap<String, usize> = BTreeMap::new();
            for c in &p.children {
                let base = c.name.clone();
                let n = seen.entry(base.clone()).or_insert(0);
                *n += 1;
                let id = if *n == 1 { base } else { format!("{base}#{n}") };
                m.defs[di].occurrences.push(Occ { id: id.clone(), name: id, def: index[c.product], transform: c.transform, visible: true, ..Default::default() });
            }
            if !p.solids.is_empty() {
                let body = m.defs.len();
                m.defs.push(Def {
                    key: keys.make(&format!("{} bodies", p.name)),
                    name: format!("{} bodies", p.name),
                    step_pd: Some(p.pd),
                    solids: p.solids.clone(),
                    ..Default::default()
                });
                m.defs[di].occurrences.insert(0, Occ { id: "bodies".into(), name: "bodies".into(), def: body, visible: true, ..Default::default() });
            }
        }
        let root = match step.roots.as_slice() {
            [one] => index[*one],
            [] => return Err("the STEP file has no products".into()),
            many => {
                // Several top products: gather them under one assembly.
                let r = m.defs.len();
                let occurrences = many
                    .iter()
                    .map(|p| Occ { id: step.products[*p].name.clone(), name: step.products[*p].name.clone(), def: index[*p], visible: true, ..Default::default() })
                    .collect();
                let name = path.file_stem().map_or("Assembly".into(), |s| s.to_string_lossy().to_string());
                m.defs.push(Def { key: keys.make(&name), name, assembly: true, occurrences, ..Default::default() });
                r
            }
        };
        m.root = root;
        // A single part file: wrap it so the root is always an assembly.
        if !m.defs[m.root].assembly {
            let part = m.root;
            m.root = m.defs.len();
            let name = format!("{} assembly", m.defs[part].name);
            m.defs.push(Def {
                key: keys.make(&name),
                name,
                assembly: true,
                occurrences: vec![Occ { id: format!("{}:1", m.defs[part].name), name: format!("{}:1", m.defs[part].name), def: part, grounded: true, visible: true, ..Default::default() }],
                ..Default::default()
            });
        }
        Ok(m)
    }

    pub fn from_bundle(b: &Bundle) -> Result<Model, String> {
        let problems = b.validate();
        if let Some(p) = problems.first() {
            return Err(format!("the bundle is inconsistent: {p}{}", if problems.len() > 1 { format!(" (and {} more)", problems.len() - 1) } else { String::new() }));
        }
        let mut m = Model { source_app: b.source.app.clone(), source_file: b.source.root_file.clone(), warnings: b.warnings.clone(), ..Default::default() };
        let mut keys = KeyMaker::default();
        let index: BTreeMap<&str, usize> = b.documents.iter().enumerate().map(|(i, d)| (d.id.as_str(), i)).collect();
        for d in &b.documents {
            let mut solids = Vec::new();
            let step = d.step.as_ref().map(|s| b.path(s));
            if d.kind == DocKind::Part {
                match &step {
                    Some(p) => match lcw_step::StepModel::read(p) {
                        Ok(sm) => {
                            for (pi, w, _) in sm.roots.iter().flat_map(|r| sm.instances(*r)) {
                                for s in &sm.products[pi].solids {
                                    let mut s = s.clone();
                                    if w != M4::IDENTITY {
                                        transform_solid(&mut s, &w);
                                    }
                                    solids.push(s);
                                }
                            }
                        }
                        Err(e) => m.warnings.push(format!("{}: {e}", d.name)),
                    },
                    None => m.warnings.push(format!("{}: the bundle has no STEP for this part; it is left out", d.name)),
                }
            }
            m.defs.push(Def {
                key: keys.make(&d.name),
                name: d.name.clone(),
                assembly: d.kind == DocKind::Assembly,
                doc: Some(d.id.clone()),
                step,
                solids,
                sheet_metal: d.sheet_metal.clone(),
                mass: d.mass.clone(),
                properties: d.properties.clone(),
                part_number: d.part_number.clone(),
                material: d.material.clone(),
                occurrences: Vec::new(),
                constraints: d.constraints.clone(),
                joints: d.joints.clone(),
                patterns: d.patterns.clone(),
                derived: d.derived.as_ref().map(|x| (x.source.clone(), x.mirrored)),
                step_pd: None,
            });
        }
        for (di, d) in b.documents.iter().enumerate() {
            m.defs[di].occurrences = d
                .occurrences
                .iter()
                .filter_map(|o| {
                    Some(Occ {
                        id: o.id.clone(),
                        name: o.name.clone(),
                        def: *index.get(o.document.as_str())?,
                        transform: o.transform,
                        grounded: o.grounded,
                        suppressed: o.suppressed,
                        visible: o.visible,
                        pattern: o.pattern.clone(),
                    })
                })
                .collect();
        }
        m.root = *index.get(b.root.as_str()).ok_or("the bundle's root is missing")?;
        if !m.defs[m.root].assembly {
            let part = m.root;
            m.root = m.defs.len();
            let name = format!("{} assembly", m.defs[part].name);
            m.defs.push(Def {
                key: keys.make(&name),
                name,
                assembly: true,
                occurrences: vec![Occ { id: format!("{}:1", m.defs[part].name), name: format!("{}:1", m.defs[part].name), def: part, grounded: true, visible: true, ..Default::default() }],
                ..Default::default()
            });
        }
        Ok(m)
    }
}

/// Move a solid's geometry by a placement.
pub fn transform_solid(s: &mut Solid, m: &M4) {
    use lcw_step::Surface;
    for f in &mut s.faces {
        for v in &mut f.vertices {
            *v = m.point(*v);
        }
        for l in &mut f.loops {
            for v in l.iter_mut() {
                *v = m.point(*v);
            }
        }
        for c in &mut f.circles {
            *c = (m.point(c.0), m.dir(c.1), c.2);
        }
        f.surface = match f.surface.clone() {
            Surface::Plane { origin, normal } => Surface::Plane { origin: m.point(origin), normal: m.dir(normal) },
            Surface::Cylinder { axis, radius } => Surface::Cylinder { axis: lcw_geom::Axis { origin: m.point(axis.origin), dir: m.dir(axis.dir) }, radius },
            Surface::Cone { axis, apex_radius, half_angle } => Surface::Cone { axis: lcw_geom::Axis { origin: m.point(axis.origin), dir: m.dir(axis.dir) }, apex_radius, half_angle },
            Surface::Sphere { center, radius } => Surface::Sphere { center: m.point(center), radius },
            Surface::Torus { axis, major, minor } => Surface::Torus { axis: lcw_geom::Axis { origin: m.point(axis.origin), dir: m.dir(axis.dir) }, major, minor },
            other => other,
        };
    }
}

/// Unique, file-system-safe keys from names.
#[derive(Default)]
pub struct KeyMaker {
    used: BTreeMap<String, usize>,
}

impl KeyMaker {
    pub fn make(&mut self, name: &str) -> String {
        let mut base: String = name
            .trim()
            .chars()
            .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') { c } else { '-' })
            .collect::<String>()
            .trim_matches(|c: char| c == '.' || c == ' ')
            .to_string();
        if base.is_empty() {
            base = "part".into();
        }
        if base.chars().count() > 80 {
            base = base.chars().take(80).collect();
        }
        let upper = base.to_uppercase();
        if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "COM1" | "LPT1") {
            base.push('_');
        }
        let n = self.used.entry(base.to_lowercase()).or_insert(0);
        *n += 1;
        if *n == 1 {
            base
        } else {
            format!("{base} ({n})")
        }
    }
}
