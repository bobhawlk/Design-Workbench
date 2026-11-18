//! Copies: different part documents that are the same part (or its mirror image).
//!
//! The same document placed many times is already one component. Here two documents count as the
//! same part when their vertices map onto each other by a rigid move, within a tolerance scaled
//! to the part's size; as mirror images when only a reflection does.

use std::collections::BTreeMap;

use lcw_geom::{register, M4};

use crate::features::PartFeatures;

#[derive(Debug, Clone)]
pub struct Copy {
    /// The part this one is a copy of (the canonical one: first in the model).
    pub of: usize,
    pub part: usize,
    /// Maps the canonical part's frame onto this part's frame (`p_this = t · p_of`).
    pub transform: M4,
    pub mirrored: bool,
}

/// Find copies among part definitions. `features[i]` is None for assemblies.
pub fn find(features: &[Option<PartFeatures>]) -> Vec<Copy> {
    let mut buckets: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    for (i, f) in features.iter().enumerate() {
        if let Some(f) = f {
            if f.vertices.len() >= 4 {
                buckets.entry((f.faces, f.vertices.len())).or_default().push(i);
            }
        }
    }
    let mut out = Vec::new();
    for group in buckets.values() {
        let mut canon: Vec<usize> = Vec::new();
        for &i in group {
            let fi = features[i].as_ref().map(|f| &f.vertices);
            let Some(vi) = fi else { continue };
            let size = features[i].as_ref().and_then(|f| f.bbox).map_or(1.0, |b| b.diagonal()).max(1.0);
            let tol = (size * 1e-5).max(1e-3);
            let mut matched = false;
            for &c in &canon {
                let Some(vc) = features[c].as_ref().map(|f| &f.vertices) else { continue };
                if let Some((t, mirrored)) = register(vc, vi, tol, true) {
                    out.push(Copy { of: c, part: i, transform: t, mirrored });
                    matched = true;
                    break;
                }
            }
            if !matched {
                canon.push(i);
            }
        }
    }
    out
}
