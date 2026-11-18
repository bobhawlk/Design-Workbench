//! The analysis between an Inventor export (or a STEP file) and a SolidCraft assembly: which
//! parts are the same, which are sheet metal, which repeat in patterns or mirror each other, and
//! how they are joined. Its output, [`Analysis`], says *what* to build; `lcw-solidcraft` says
//! *how* in the SolidCraft version at hand.

pub mod features;
pub mod joints;
pub mod model;
pub mod patterns;
pub mod repeats;
pub mod sheet;
pub mod symmetry;

use std::collections::BTreeMap;

use features::PartFeatures;
use model::{Inst, Model};

/// When two different part documents are the same part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// Keep every document a component of its own; report the copies.
    Report,
    /// Use one component for copies whose names differ only by a copy suffix
    /// ("Bracket", "Bracket_2", "Bracket (1)").
    SameName,
    /// Use one component for every exact copy.
    All,
}

pub struct Options {
    pub sheet: sheet::Options,
    pub joints: joints::Options,
    pub merge: Merge,
    pub detect_patterns: bool,
    pub detect_symmetry: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options { sheet: sheet::Options::default(), joints: joints::Options::default(), merge: Merge::SameName, detect_patterns: true, detect_symmetry: true }
    }
}

#[derive(Debug, Clone)]
pub struct PatternPlan {
    /// The assembly definition holding the members.
    pub assembly: usize,
    pub name: String,
    pub found: patterns::Found,
    /// Inventor's own pattern (true) or recognised from placements.
    pub from_inventor: bool,
}

#[derive(Debug, Clone)]
pub struct Mirror {
    pub part: usize,
    pub of: usize,
}

pub struct Analysis {
    pub model: Model,
    pub instances: Vec<Inst>,
    pub features: Vec<Option<PartFeatures>>,
    /// Exact copies that were merged into one component (part → canonical part).
    pub merged: Vec<(usize, usize)>,
    /// Copies left as their own components (reported).
    pub copies: Vec<repeats::Copy>,
    pub mirrors: Vec<Mirror>,
    pub sheets: BTreeMap<usize, sheet::SheetPlan>,
    pub patterns: Vec<PatternPlan>,
    pub symmetry: Vec<symmetry::Plane>,
    pub joints: joints::Plan,
    pub notes: Vec<String>,
}

/// Strip a copy suffix: "Bracket_2", "Bracket (1)", "Bracket-copy" → "bracket".
pub fn base_name(name: &str) -> String {
    let mut s = name.trim().to_lowercase();
    for sep in [" (", "_", "-", " "] {
        if let Some(i) = s.rfind(sep) {
            let tail = s[i + sep.len()..].trim_end_matches(')');
            if !tail.is_empty() && (tail.chars().all(|c| c.is_ascii_digit()) || tail == "copy") {
                s.truncate(i);
            }
        }
    }
    s
}

pub fn analyze(mut model: Model, opts: &Options) -> Analysis {
    let mut notes = Vec::new();
    let mut features: Vec<Option<PartFeatures>> =
        model.defs.iter().map(|d| (!d.assembly && !d.solids.is_empty()).then(|| PartFeatures::of(&d.solids))).collect();
    // Copies between documents.
    let all_copies = repeats::find(&features);
    let mut merged = Vec::new();
    let mut copies = Vec::new();
    let mut mirrors = Vec::new();
    for c in all_copies {
        if c.mirrored {
            mirrors.push(Mirror { part: c.part, of: c.of });
            continue;
        }
        let same_name = base_name(&model.defs[c.part].name) == base_name(&model.defs[c.of].name);
        let merge = match opts.merge {
            Merge::All => true,
            Merge::SameName => same_name,
            Merge::Report => false,
        };
        if merge {
            merged.push((c.part, c.of));
            // Place the canonical part where the copy was: p_copy = t · p_canon.
            for d in &mut model.defs {
                for o in &mut d.occurrences {
                    if o.def == c.part {
                        o.def = c.of;
                        o.transform = o.transform * c.transform;
                    }
                }
            }
        } else {
            copies.push(c);
        }
    }
    if !merged.is_empty() {
        notes.push(format!("{} part document(s) are exact copies of another and share its component", merged.len()));
    }
    for (part, _) in &merged {
        features[*part] = None;
    }
    // Derived mirror parts named by Inventor count too.
    for (i, d) in model.defs.iter().enumerate() {
        if let Some((src, true)) = &d.derived {
            if let Some(of) = model.defs.iter().position(|x| x.doc.as_deref() == Some(src.as_str())) {
                if !mirrors.iter().any(|m| m.part == i) {
                    mirrors.push(Mirror { part: i, of });
                }
            }
        }
    }
    let instances = model.instances();
    let used: std::collections::BTreeSet<usize> = instances.iter().map(|i| i.def).collect();
    // Sheet metal.
    let mut sheets = BTreeMap::new();
    for (i, d) in model.defs.iter().enumerate() {
        if d.assembly || !used.contains(&i) {
            continue;
        }
        if let Some(p) = sheet::plan(&d.solids, d.sheet_metal.as_ref(), d.material.as_deref(), &opts.sheet) {
            sheets.insert(i, p);
        } else if d.sheet_metal.is_some() {
            notes.push(format!("{}: Inventor calls it sheet metal but it could not be read as one sheet; it comes in as a solid", d.name));
        }
    }
    // Patterns per assembly.
    let mut pats = Vec::new();
    for (ai, d) in model.defs.iter().enumerate().filter(|(_, d)| d.assembly) {
        for p in &d.patterns {
            let members: Vec<usize> = p.elements.iter().filter_map(|e| e.first()).filter_map(|id| d.occurrences.iter().position(|o| &o.id == id)).collect();
            if members.len() >= 2 {
                let placements: Vec<(usize, lcw_geom::M4)> = members.iter().map(|i| (*i, d.occurrences[*i].transform)).collect();
                match patterns::detect(&placements, 1e-3).filter(|_| members.len() >= 3) {
                    Some(found) => pats.push(PatternPlan { assembly: ai, name: p.name.clone(), found, from_inventor: true }),
                    None => notes.push(format!("{} › {}: Inventor pattern of {} kept as plain placements", d.name, p.name, members.len())),
                }
            }
        }
        if !opts.detect_patterns {
            continue;
        }
        let mut by_def: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, o) in d.occurrences.iter().enumerate() {
            if !o.suppressed && o.pattern.is_none() {
                by_def.entry(o.def).or_default().push(i);
            }
        }
        for (def, occs) in by_def {
            if occs.len() < 3 || pats.iter().any(|p| p.assembly == ai && p.found.members.iter().any(|m| occs.contains(m))) {
                continue;
            }
            let placements: Vec<(usize, lcw_geom::M4)> = occs.iter().map(|i| (*i, d.occurrences[*i].transform)).collect();
            if let Some(found) = patterns::detect(&placements, 1e-3) {
                pats.push(PatternPlan { assembly: ai, name: format!("{} pattern", model.defs[def].name), found, from_inventor: false });
            }
        }
    }
    // Symmetry.
    let symmetry = if opts.detect_symmetry {
        let parts: Vec<(usize, lcw_geom::M4)> = instances.iter().filter(|x| features[x.def].is_some()).map(|x| (x.def, x.world)).collect();
        let mirror_pairs: Vec<(usize, usize)> = mirrors.iter().map(|m| (m.part, m.of)).collect();
        let same = |a: usize, b: usize| a == b || mirror_pairs.iter().any(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a));
        symmetry::planes(&parts, &features, same, 2)
    } else {
        Vec::new()
    };
    let joints = joints::plan(&model, &instances, &features, &opts.joints);
    notes.extend(joints.notes.iter().cloned());
    Analysis { model, instances, features, merged, copies, mirrors, sheets, patterns: pats, symmetry, joints, notes }
}
