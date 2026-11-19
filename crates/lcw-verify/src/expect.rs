//! What Inventor had, read from the bundle alone: every placed occurrence at any depth with its
//! world placement (the product of the placements down its path), computed here and not taken
//! from the converter.

use lcw_bundle::{Bundle, DocKind};
use lcw_geom::M4;

#[derive(Debug, Clone)]
pub struct ExpInst {
    /// Inventor's occurrence names from the top assembly down to this one.
    pub names: Vec<String>,
    pub doc: String,
    pub doc_name: String,
    pub is_assembly: bool,
    pub world: M4,
    /// The name of the assembly holding it (the root's for a top-level occurrence).
    pub parent_name: String,
    /// The part has a STEP in the bundle (an assembly: true).
    pub has_step: bool,
}

impl ExpInst {
    pub fn name(&self) -> &str {
        self.names.last().map(String::as_str).unwrap_or("")
    }
}

#[derive(Debug, Clone, Default)]
pub struct Expected {
    pub insts: Vec<ExpInst>,
    /// Occurrences left out because Inventor suppressed them or they have no document.
    pub left_out: usize,
    /// Parts placed whose bundle has no STEP (their file was missing or the export failed):
    /// the converter cannot build them. (document name, placements)
    pub no_step: Vec<(String, usize)>,
}

pub fn expand(b: &Bundle) -> Result<Expected, String> {
    let root = b.root_doc().ok_or_else(|| format!("the bundle's root `{}` is not a document", b.root))?;
    let mut e = Expected::default();
    walk(b, root, &M4::IDENTITY, &[], &mut e, 0);
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for i in &e.insts {
        if !i.is_assembly && !i.has_step {
            *counts.entry(i.doc_name.clone()).or_default() += 1;
        }
    }
    e.no_step = counts.into_iter().collect();
    Ok(e)
}

fn walk(b: &Bundle, doc: &lcw_bundle::Document, world: &M4, names: &[String], out: &mut Expected, depth: usize) {
    if depth > 32 {
        return;
    }
    for o in &doc.occurrences {
        let Some(d) = b.doc(&o.document).filter(|_| !o.suppressed) else {
            out.left_out += 1;
            continue;
        };
        let w = *world * o.transform;
        let mut n = names.to_vec();
        n.push(o.name.clone());
        out.insts.push(ExpInst {
            names: n.clone(),
            doc: d.id.clone(),
            doc_name: d.name.clone(),
            is_assembly: d.kind == DocKind::Assembly,
            world: w,
            parent_name: doc.name.clone(),
            has_step: d.kind == DocKind::Assembly || d.step.is_some(),
        });
        if d.kind == DocKind::Assembly {
            walk(b, d, &w, &n, out, depth + 1);
        }
    }
}
