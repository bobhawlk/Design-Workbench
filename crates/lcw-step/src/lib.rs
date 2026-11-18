//! A STEP (ISO 10303-21) reader for what the bridge needs: the product tree with placements, and
//! a summary of each part's B-rep (faces with their surface type, outward normals and vertices,
//! and which faces share edges). It is not a modelling kernel: SolidCraft rebuilds the exact
//! geometry; this reader only lets the analysis see hole axes, plate faces and bends.
//!
//! Placements follow SolidCraft's STEP importer: a component's placement in its parent is
//! `M(item₂) · M(item₁)⁻¹` of the `ITEM_DEFINED_TRANSFORMATION` when the component's shape
//! representation is the relationship's first one (the usual case), the inverse otherwise.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use lcw_geom::{Axis, Bbox, M4, V3};

#[derive(Debug, Clone, PartialEq)]
pub struct StepError(pub String);

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StepError {}

/// A parameter of a STEP entity.
#[derive(Debug, Clone, PartialEq)]
pub enum P {
    Ref(u64),
    Num(f64),
    Str(String),
    Enum(String),
    List(Vec<P>),
    /// `$`
    Unset,
    /// `*`
    Derived,
    /// `NAME(value)` inside a parameter list.
    Typed(String, Vec<P>),
}

impl P {
    pub fn as_ref(&self) -> Option<u64> {
        match self {
            P::Ref(r) => Some(*r),
            _ => None,
        }
    }
    pub fn as_num(&self) -> Option<f64> {
        match self {
            P::Num(n) => Some(*n),
            P::Typed(_, v) => v.first()?.as_num(),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            P::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_enum(&self) -> Option<&str> {
        match self {
            P::Enum(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_list(&self) -> &[P] {
        match self {
            P::List(v) => v,
            _ => &[],
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self.as_enum()? {
            "T" => Some(true),
            "F" => Some(false),
            _ => None,
        }
    }
    /// Every reference anywhere inside this parameter.
    pub fn refs(&self, out: &mut Vec<u64>) {
        match self {
            P::Ref(r) => out.push(*r),
            P::List(v) | P::Typed(_, v) => v.iter().for_each(|p| p.refs(out)),
            _ => {}
        }
    }
}

/// One entity instance: a simple `NAME(params)` or a complex `(A(..) B(..))` one.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    /// Simple: one part. Complex: each partial entity, in file order.
    pub parts: Vec<(String, Vec<P>)>,
}

impl Entity {
    pub fn name(&self) -> &str {
        self.parts.first().map_or("", |p| p.0.as_str())
    }
    pub fn params(&self) -> &[P] {
        self.parts.first().map_or(&[], |p| p.1.as_slice())
    }
    pub fn part(&self, name: &str) -> Option<&[P]> {
        self.parts.iter().find(|p| p.0 == name).map(|p| p.1.as_slice())
    }
    pub fn is(&self, name: &str) -> bool {
        self.parts.iter().any(|p| p.0 == name)
    }
    pub fn refs(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for (_, ps) in &self.parts {
            ps.iter().for_each(|p| p.refs(&mut out));
        }
        out
    }
}

/// Decode STEP string escapes (`''`, `\X2\…\X0\`, `\X\hh`, `\S\c`).
fn decode_str(raw: &str) -> String {
    let raw = raw.replace("''", "'");
    if !raw.contains('\\') {
        return raw;
    }
    let mut out = String::new();
    let b: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '\\' && i + 3 < b.len() && b[i + 1] == 'X' && b[i + 2] == '2' && b[i + 3] == '\\' {
            i += 4;
            while i + 3 < b.len() && !(b[i] == '\\' && b[i + 1] == 'X' && b[i + 2] == '0' && b[i + 3] == '\\') {
                let hex: String = b[i..(i + 4).min(b.len())].iter().collect();
                if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(c);
                }
                i += 4;
            }
            i += 4;
        } else if b[i] == '\\' && i + 4 < b.len() && b[i + 1] == 'X' && b[i + 2] == '\\' {
            let hex: String = b[i + 3..i + 5].iter().collect();
            if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(c);
            }
            i += 5;
        } else if b[i] == '\\' && i + 3 < b.len() && b[i + 1] == 'S' && b[i + 2] == '\\' {
            out.push(char::from_u32(b[i + 3] as u32 + 128).unwrap_or('?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl Lexer<'_> {
    fn ws(&mut self) {
        loop {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"/*") {
                match find(&self.s[self.i + 2..], b"*/") {
                    Some(n) => self.i += n + 4,
                    None => self.i = self.s.len(),
                }
            } else {
                return;
            }
        }
    }
    fn ident(&mut self) -> String {
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).to_ascii_uppercase()
    }
    fn list(&mut self) -> Result<Vec<P>, String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("nesting too deep".into());
        }
        // at '('
        self.i += 1;
        let mut out = Vec::new();
        loop {
            self.ws();
            match self.s.get(self.i) {
                None => return Err("unterminated list".into()),
                Some(b')') => {
                    self.i += 1;
                    break;
                }
                Some(b',') => {
                    self.i += 1;
                }
                Some(_) => out.push(self.param()?),
            }
        }
        self.depth -= 1;
        Ok(out)
    }
    fn param(&mut self) -> Result<P, String> {
        self.ws();
        let c = *self.s.get(self.i).ok_or("unexpected end")?;
        match c {
            b'#' => {
                self.i += 1;
                let start = self.i;
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1;
                }
                let n = std::str::from_utf8(&self.s[start..self.i]).ok().and_then(|t| t.parse().ok()).ok_or("bad reference")?;
                Ok(P::Ref(n))
            }
            b'\'' => {
                self.i += 1;
                let start = self.i;
                loop {
                    match self.s.get(self.i) {
                        None => return Err("unterminated string".into()),
                        Some(b'\'') if self.s.get(self.i + 1) == Some(&b'\'') => self.i += 2,
                        Some(b'\'') => break,
                        _ => self.i += 1,
                    }
                }
                let raw = String::from_utf8_lossy(&self.s[start..self.i]).to_string();
                self.i += 1;
                Ok(P::Str(decode_str(&raw)))
            }
            b'.' => {
                self.i += 1;
                let start = self.i;
                while self.i < self.s.len() && self.s[self.i] != b'.' {
                    self.i += 1;
                }
                let e = String::from_utf8_lossy(&self.s[start..self.i]).to_string();
                self.i += 1;
                Ok(P::Enum(e))
            }
            b'$' => {
                self.i += 1;
                Ok(P::Unset)
            }
            b'*' => {
                self.i += 1;
                Ok(P::Derived)
            }
            b'(' => Ok(P::List(self.list()?)),
            b'"' => {
                // binary: skip
                self.i += 1;
                while self.i < self.s.len() && self.s[self.i] != b'"' {
                    self.i += 1;
                }
                self.i += 1;
                Ok(P::Unset)
            }
            b'-' | b'+' | b'0'..=b'9' => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len() && matches!(self.s[self.i], b'0'..=b'9' | b'.' | b'E' | b'e' | b'+' | b'-') {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| "bad number")?;
                Ok(P::Num(t.parse::<f64>().map_err(|_| format!("bad number `{t}`"))?))
            }
            c if c.is_ascii_alphabetic() => {
                let name = self.ident();
                self.ws();
                let args = if self.s.get(self.i) == Some(&b'(') { self.list()? } else { Vec::new() };
                Ok(P::Typed(name, args))
            }
            _ => Err(format!("unexpected `{}`", c as char)),
        }
    }
    fn entity(&mut self) -> Result<Entity, String> {
        self.ws();
        if self.s.get(self.i) == Some(&b'(') {
            // complex entity
            self.i += 1;
            let mut parts = Vec::new();
            loop {
                self.ws();
                match self.s.get(self.i) {
                    Some(b')') => {
                        self.i += 1;
                        break;
                    }
                    Some(c) if c.is_ascii_alphabetic() => {
                        let name = self.ident();
                        self.ws();
                        let args = if self.s.get(self.i) == Some(&b'(') { self.list()? } else { Vec::new() };
                        parts.push((name, args));
                    }
                    _ => return Err("bad complex entity".into()),
                }
            }
            Ok(Entity { parts })
        } else {
            let name = self.ident();
            self.ws();
            let args = if self.s.get(self.i) == Some(&b'(') { self.list()? } else { Vec::new() };
            Ok(Entity { parts: vec![(name, args)] })
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Every entity of the file's DATA section, by instance number.
pub fn parse_entities(text: &str) -> Result<HashMap<u64, Entity>, StepError> {
    parse_entities_with_spans(text).map(|(e, _)| e)
}

/// Entities and each one's byte range in the text (`#id=…;`), for copying entities verbatim.
pub fn parse_entities_with_spans(text: &str) -> Result<(HashMap<u64, Entity>, HashMap<u64, (usize, usize)>), StepError> {
    let mut spans = HashMap::new();
    let bytes = text.as_bytes();
    let data = find(bytes, b"DATA;").ok_or_else(|| StepError("no DATA section: not a STEP file".into()))? + 5;
    let mut lx = Lexer { s: bytes, i: data, depth: 0 };
    let mut out = HashMap::new();
    loop {
        lx.ws();
        if lx.i >= bytes.len() || bytes[lx.i..].starts_with(b"ENDSEC") {
            break;
        }
        if bytes[lx.i] != b'#' {
            // Skip anything we don't understand up to the next statement.
            match bytes[lx.i..].iter().position(|&c| c == b';') {
                Some(n) => {
                    lx.i += n + 1;
                    continue;
                }
                None => break,
            }
        }
        let stmt_start = lx.i;
        lx.i += 1;
        let start = lx.i;
        while lx.i < bytes.len() && bytes[lx.i].is_ascii_digit() {
            lx.i += 1;
        }
        let id: u64 = std::str::from_utf8(&bytes[start..lx.i]).ok().and_then(|t| t.parse().ok()).ok_or_else(|| StepError(format!("bad instance at byte {start}")))?;
        lx.ws();
        if bytes.get(lx.i) != Some(&b'=') {
            return Err(StepError(format!("#{id}: expected '='")));
        }
        lx.i += 1;
        lx.depth = 0;
        let e = lx.entity().map_err(|m| StepError(format!("#{id}: {m}")))?;
        lx.ws();
        if bytes.get(lx.i) == Some(&b';') {
            lx.i += 1;
        }
        spans.insert(id, (stmt_start, lx.i));
        out.insert(id, e);
    }
    Ok((out, spans))
}

/// The surface a face lies on, in the part's frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Surface {
    Plane { origin: V3, normal: V3 },
    Cylinder { axis: Axis, radius: f64 },
    Cone { axis: Axis, apex_radius: f64, half_angle: f64 },
    Sphere { center: V3, radius: f64 },
    Torus { axis: Axis, major: f64, minor: f64 },
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub surface: Surface,
    /// The face's normal agrees with the surface's (STEP `same_sense`).
    pub same_sense: bool,
    /// Edge instance numbers of all loops (shared edges join faces).
    pub edges: Vec<u64>,
    /// Vertex positions of all loops (outer loop first), part frame.
    pub vertices: Vec<V3>,
    /// Each loop's vertices (outer loop first).
    pub loops: Vec<Vec<V3>>,
    /// Round edges (circles) bounding the face: centre, axis and radius.
    pub circles: Vec<(V3, V3, f64)>,
}

impl Face {
    /// Outward normal of a planar face.
    pub fn plane_normal(&self) -> Option<V3> {
        match self.surface {
            Surface::Plane { normal, .. } => Some(if self.same_sense { normal } else { -normal }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Solid {
    pub name: String,
    pub faces: Vec<Face>,
}

impl Solid {
    pub fn vertices(&self) -> Vec<V3> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for f in &self.faces {
            for v in &f.vertices {
                let key = (v.round(1e-6).x.to_bits(), v.round(1e-6).y.to_bits(), v.round(1e-6).z.to_bits());
                if seen.insert(key) {
                    out.push(*v);
                }
            }
        }
        out
    }
    pub fn bbox(&self) -> Bbox {
        Bbox::of(self.faces.iter().flat_map(|f| f.vertices.iter().copied()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    pub product: usize,
    /// Placement in the parent product's frame.
    pub transform: M4,
    /// The occurrence's name in the file (NAUO id or name).
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Product {
    /// The PRODUCT_DEFINITION instance number in the file.
    pub pd: u64,
    pub name: String,
    /// The product's id string (PRODUCT's first field: often the part number).
    pub id: String,
    pub children: Vec<Child>,
    pub solids: Vec<Solid>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StepModel {
    pub products: Vec<Product>,
    /// Products no other product places.
    pub roots: Vec<usize>,
    /// Problems met while reading (a placement that could not be read, an unsupported surface).
    pub warnings: Vec<String>,
}

impl StepModel {
    pub fn read(path: &std::path::Path) -> Result<StepModel, StepError> {
        let bytes = std::fs::read(path).map_err(|e| StepError(format!("{}: {e}", path.display())))?;
        StepModel::parse(&String::from_utf8_lossy(&bytes))
    }

    pub fn parse(text: &str) -> Result<StepModel, StepError> {
        let ents = parse_entities(text)?;
        Reader { e: &ents, warnings: Vec::new(), unit_cache: HashMap::new() }.model()
    }

    /// Every placement of every part below `root`, with its world placement and the occurrence
    /// path of names: (product index, world placement, path).
    pub fn instances(&self, root: usize) -> Vec<(usize, M4, Vec<String>)> {
        let mut out = Vec::new();
        let mut stack = vec![(root, M4::IDENTITY, Vec::new(), 0usize)];
        while let Some((p, m, path, depth)) = stack.pop() {
            if depth > 32 {
                continue;
            }
            out.push((p, m, path.clone()));
            for c in self.products[p].children.iter().rev() {
                let mut np = path.clone();
                np.push(c.name.clone());
                stack.push((c.product, m * c.transform, np, depth + 1));
            }
        }
        out
    }
}

/// A parsed STEP file that keeps its text, so parts can be written out on their own.
pub struct StepFile {
    pub text: String,
    pub entities: HashMap<u64, Entity>,
    spans: HashMap<u64, (usize, usize)>,
}

impl StepFile {
    pub fn read(path: &std::path::Path) -> Result<StepFile, StepError> {
        let bytes = std::fs::read(path).map_err(|e| StepError(format!("{}: {e}", path.display())))?;
        StepFile::parse(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn parse(text: String) -> Result<StepFile, StepError> {
        let (entities, spans) = parse_entities_with_spans(&text)?;
        Ok(StepFile { text, entities, spans })
    }

    pub fn model(&self) -> Result<StepModel, StepError> {
        Reader { e: &self.entities, warnings: Vec::new(), unit_cache: HashMap::new() }.model()
    }

    /// A STEP file holding one product (by its PRODUCT_DEFINITION number) on its own, in its own
    /// frame: its definition, its shape representations and their geometry, copied verbatim.
    /// The product's children are left out: write each part separately.
    pub fn extract_product(&self, pd: u64) -> Result<String, StepError> {
        if !self.entities.get(&pd).is_some_and(|e| e.is("PRODUCT_DEFINITION")) {
            return Err(StepError(format!("#{pd} is not a PRODUCT_DEFINITION")));
        }
        let mut seeds = vec![pd];
        let pds: Vec<u64> = self
            .entities
            .iter()
            .filter(|(_, e)| e.is("PRODUCT_DEFINITION_SHAPE") && e.params().get(2).and_then(P::as_ref) == Some(pd))
            .map(|(k, _)| *k)
            .collect();
        seeds.extend(&pds);
        let mut reps = Vec::new();
        for (k, e) in &self.entities {
            if e.is("SHAPE_DEFINITION_REPRESENTATION") && e.params().first().and_then(P::as_ref).is_some_and(|d| pds.contains(&d)) {
                seeds.push(*k);
                if let Some(r) = e.params().get(1).and_then(P::as_ref) {
                    reps.push(r);
                }
            }
        }
        // Representations of other products (to stop at assembly links).
        let foreign: HashSet<u64> = self
            .entities
            .values()
            .filter(|e| e.is("SHAPE_DEFINITION_REPRESENTATION"))
            .filter_map(|e| e.params().get(1).and_then(P::as_ref))
            .filter(|r| !reps.contains(r))
            .collect();
        let mut i = 0;
        while i < reps.len() {
            let r = reps[i];
            for (k, e) in &self.entities {
                if e.is("SHAPE_REPRESENTATION_RELATIONSHIP") && !e.is("REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION") {
                    let refs: Vec<u64> = e.parts.iter().find(|p| p.1.len() >= 4).map(|p| p.1.iter().filter_map(P::as_ref).collect()).unwrap_or_default();
                    if refs.len() >= 2 && refs.contains(&r) {
                        let other = if refs[0] == r { refs[1] } else { refs[0] };
                        if !foreign.contains(&other) {
                            seeds.push(*k);
                            if !reps.contains(&other) {
                                reps.push(other);
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        seeds.extend(&reps);
        // Closure over references.
        let mut keep: HashSet<u64> = HashSet::new();
        let mut stack = seeds;
        while let Some(id) = stack.pop() {
            if !keep.insert(id) {
                continue;
            }
            if let Some(e) = self.entities.get(&id) {
                stack.extend(e.refs());
            }
        }
        let schema = self
            .text
            .find("FILE_SCHEMA")
            .and_then(|a| self.text[a..].find(';').map(|b| self.text[a..a + b + 1].to_string()))
            .unwrap_or_else(|| "FILE_SCHEMA(('AUTOMOTIVE_DESIGN { 1 0 10303 214 1 1 1 1 }'));".into());
        let mut ids: Vec<u64> = keep.into_iter().collect();
        ids.sort_unstable();
        let mut out = String::with_capacity(ids.len() * 80);
        out.push_str("ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('one part extracted by lcw-step'),'2;1');\nFILE_NAME('','',(''),(''),'lcw-step','','');\n");
        out.push_str(&schema);
        out.push_str("\nENDSEC;\nDATA;\n");
        for id in ids {
            if let Some((a, b)) = self.spans.get(&id) {
                out.push_str(&self.text[*a..*b]);
                out.push('\n');
            }
        }
        out.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        Ok(out)
    }
}

struct Reader<'a> {
    e: &'a HashMap<u64, Entity>,
    warnings: Vec<String>,
    unit_cache: HashMap<u64, f64>,
}

impl<'a> Reader<'a> {
    fn get(&self, id: u64) -> Option<&'a Entity> {
        self.e.get(&id)
    }
    fn of_type<'b>(&self, name: &'b str) -> impl Iterator<Item = (u64, &'a Entity)> + 'b
    where
        'a: 'b,
    {
        let e: &'a HashMap<u64, Entity> = self.e;
        e.iter().filter(move |(_, e)| e.is(name)).map(|(k, v)| (*k, v))
    }

    fn point(&self, id: u64, scale: f64) -> Option<V3> {
        let e = self.get(id)?;
        let c: Vec<f64> = e.part("CARTESIAN_POINT")?.get(1)?.as_list().iter().filter_map(P::as_num).collect();
        Some(V3::from_array([c.first().copied()?, c.get(1).copied().unwrap_or(0.0), c.get(2).copied().unwrap_or(0.0)]) * scale)
    }
    fn direction(&self, id: u64) -> Option<V3> {
        let e = self.get(id)?;
        let c: Vec<f64> = e.part("DIRECTION")?.get(1)?.as_list().iter().filter_map(P::as_num).collect();
        V3::from_array([c.first().copied()?, c.get(1).copied().unwrap_or(0.0), c.get(2).copied().unwrap_or(0.0)]).unit()
    }
    /// An AXIS2_PLACEMENT_3D as a placement matrix.
    fn placement(&self, id: u64, scale: f64) -> Option<M4> {
        let e = self.get(id)?;
        let p = e.part("AXIS2_PLACEMENT_3D")?;
        let origin = self.point(p.get(1)?.as_ref()?, scale)?;
        let z = p.get(2).and_then(P::as_ref).and_then(|d| self.direction(d)).unwrap_or(V3::Z);
        let x = p.get(3).and_then(P::as_ref).and_then(|d| self.direction(d)).unwrap_or_else(|| z.any_perp());
        M4::frame(origin, z, x)
    }

    /// Millimetres per length unit of a representation context (1 when unknown).
    fn context_scale(&mut self, ctx: u64) -> f64 {
        if let Some(s) = self.unit_cache.get(&ctx) {
            return *s;
        }
        let mut scale = 1.0;
        if let Some(units) = self.get(ctx).and_then(|e| e.part("GLOBAL_UNIT_ASSIGNED_CONTEXT")).and_then(|p| p.first()) {
            for u in units.as_list().iter().filter_map(P::as_ref) {
                if let Some(s) = self.length_unit(u, 0) {
                    scale = s;
                }
            }
        }
        self.unit_cache.insert(ctx, scale);
        scale
    }
    fn length_unit(&self, u: u64, depth: usize) -> Option<f64> {
        let e = self.get(u)?;
        if depth > 4 || !e.is("LENGTH_UNIT") {
            return None;
        }
        if let Some(si) = e.part("SI_UNIT") {
            let prefix = si.first().and_then(P::as_enum).unwrap_or("");
            let base = match si.get(1).and_then(P::as_enum) {
                Some("METRE") => 1000.0,
                _ => return None,
            };
            let f = match prefix {
                "MILLI" => 1e-3,
                "CENTI" => 1e-2,
                "DECI" => 1e-1,
                "MICRO" => 1e-6,
                "KILO" => 1e3,
                _ => 1.0,
            };
            return Some(base * f);
        }
        if let Some(cb) = e.part("CONVERSION_BASED_UNIT") {
            let m = self.get(cb.get(1)?.as_ref()?)?;
            let (name, params) = m.parts.first()?;
            if !name.contains("MEASURE_WITH_UNIT") {
                return None;
            }
            let factor = params.first()?.as_num()?;
            let base = self.length_unit(params.get(1)?.as_ref()?, depth + 1)?;
            return Some(factor * base);
        }
        None
    }

    fn model(mut self) -> Result<StepModel, StepError> {
        // PRODUCT_DEFINITION → product name and id.
        let mut products: Vec<Product> = Vec::new();
        let mut pd_index: BTreeMap<u64, usize> = BTreeMap::new();
        let mut pds: Vec<u64> = self.of_type("PRODUCT_DEFINITION").map(|(k, _)| k).collect();
        pds.sort_unstable();
        for pd in pds {
            let Some(e) = self.get(pd) else { continue };
            let pdf = e.params().get(2).and_then(P::as_ref);
            let product = pdf.and_then(|f| self.get(f)).and_then(|f| f.params().get(2).and_then(P::as_ref));
            let (id, name) = product
                .and_then(|p| self.get(p))
                .map(|p| {
                    let ps = p.params();
                    (ps.first().and_then(P::as_str).unwrap_or("").to_string(), ps.get(1).and_then(P::as_str).unwrap_or("").to_string())
                })
                .unwrap_or_default();
            let name = if name.trim().is_empty() { id.clone() } else { name };
            pd_index.insert(pd, products.len());
            products.push(Product { pd, name, id, children: Vec::new(), solids: Vec::new() });
        }
        // PRODUCT_DEFINITION_SHAPE → its definition (a PD or a NAUO).
        let mut pds_of: HashMap<u64, u64> = HashMap::new();
        for (k, e) in self.of_type("PRODUCT_DEFINITION_SHAPE") {
            if let Some(d) = e.params().get(2).and_then(P::as_ref) {
                pds_of.insert(k, d);
            }
        }
        // Shape representations of each PD (SDR), then related representations (SRR) holding breps.
        let mut reps_of_pd: HashMap<u64, Vec<u64>> = HashMap::new();
        for (_, e) in self.of_type("SHAPE_DEFINITION_REPRESENTATION") {
            let ps = e.params();
            if let (Some(d), Some(r)) = (ps.first().and_then(P::as_ref), ps.get(1).and_then(P::as_ref)) {
                if let Some(pd) = pds_of.get(&d) {
                    if pd_index.contains_key(pd) {
                        reps_of_pd.entry(*pd).or_default().push(r);
                    }
                }
            }
        }
        let mut related: HashMap<u64, Vec<u64>> = HashMap::new();
        for e in self.e.values() {
            if e.is("SHAPE_REPRESENTATION_RELATIONSHIP") && !e.is("REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION") {
                let ps = e.part("SHAPE_REPRESENTATION_RELATIONSHIP").or_else(|| e.part("REPRESENTATION_RELATIONSHIP")).unwrap_or(&[]);
                let ps = if ps.len() >= 4 { ps } else { e.part("REPRESENTATION_RELATIONSHIP").unwrap_or(ps) };
                let refs: Vec<u64> = ps.iter().filter_map(P::as_ref).collect();
                if refs.len() >= 2 {
                    related.entry(refs[0]).or_default().push(refs[1]);
                    related.entry(refs[1]).or_default().push(refs[0]);
                }
            }
        }
        // Solids per product.
        let pd_list: Vec<(u64, usize)> = pd_index.iter().map(|(k, v)| (*k, *v)).collect();
        for (pd, idx) in &pd_list {
            let mut reps: Vec<u64> = reps_of_pd.get(pd).cloned().unwrap_or_default();
            let mut seen: HashSet<u64> = reps.iter().copied().collect();
            let mut i = 0;
            while i < reps.len() {
                for r in related.get(&reps[i]).cloned().unwrap_or_default() {
                    // Only follow plain relationships of this product, not assembly links of children.
                    if seen.insert(r) && !reps_of_pd.iter().any(|(other, rs)| other != pd && rs.contains(&r)) {
                        reps.push(r);
                    }
                }
                i += 1;
            }
            let mut solids = Vec::new();
            for r in reps {
                let Some(rep) = self.get(r) else { continue };
                let (_, ps) = &rep.parts[0];
                let ctx = ps.get(2).and_then(P::as_ref).or_else(|| rep.parts.iter().find_map(|(_, p)| p.get(2).and_then(P::as_ref)));
                let scale = ctx.map_or(1.0, |c| self.context_scale(c));
                let items: Vec<u64> = ps.get(1).map(|l| l.as_list().iter().filter_map(P::as_ref).collect()).unwrap_or_default();
                for it in items {
                    if let Some(s) = self.solid(it, scale) {
                        solids.push(s);
                    }
                }
            }
            products[*idx].solids = solids;
        }
        // Assembly structure: NAUO (relating = parent PD, related = child PD) and its placement.
        let mut nauo_children: Vec<(u64, u64, u64, String)> = Vec::new(); // (nauo, parent pd, child pd, name)
        for (k, e) in self.of_type("NEXT_ASSEMBLY_USAGE_OCCURRENCE") {
            let ps = e.part("NEXT_ASSEMBLY_USAGE_OCCURRENCE").unwrap_or(e.params());
            let id = ps.first().and_then(P::as_str).unwrap_or("").to_string();
            let name = ps.get(1).and_then(P::as_str).unwrap_or("").to_string();
            let (Some(parent), Some(child)) = (ps.get(3).and_then(P::as_ref), ps.get(4).and_then(P::as_ref)) else { continue };
            let label = if !name.trim().is_empty() { name } else { id };
            nauo_children.push((k, parent, child, label));
        }
        nauo_children.sort_unstable_by_key(|n| n.0);
        // CDSR: (representation relationship, PDS of the NAUO).
        let mut cdsr_of_nauo: HashMap<u64, u64> = HashMap::new();
        for (_, e) in self.of_type("CONTEXT_DEPENDENT_SHAPE_REPRESENTATION") {
            let ps = e.params();
            if let (Some(rr), Some(pds)) = (ps.first().and_then(P::as_ref), ps.get(1).and_then(P::as_ref)) {
                if let Some(n) = pds_of.get(&pds) {
                    cdsr_of_nauo.insert(*n, rr);
                }
            }
        }
        let mut placed = HashSet::new();
        for (nauo, parent, child, label) in nauo_children {
            let (Some(&pi), Some(&ci)) = (pd_index.get(&parent), pd_index.get(&child)) else { continue };
            let child_reps: Vec<u64> = reps_of_pd.get(&child).cloned().unwrap_or_default();
            let transform = match cdsr_of_nauo.get(&nauo) {
                Some(rr) => self.relationship_transform(*rr, &child_reps).unwrap_or_else(|m| {
                    self.warnings.push(format!("placement of `{label}`: {m} (placed at the origin)"));
                    M4::IDENTITY
                }),
                None => M4::IDENTITY,
            };
            products[pi].children.push(Child { product: ci, transform, name: label });
            placed.insert(ci);
        }
        let roots = (0..products.len()).filter(|i| !placed.contains(i)).collect();
        Ok(StepModel { products, roots, warnings: self.warnings })
    }

    fn relationship_transform(&mut self, rr: u64, child_reps: &[u64]) -> Result<M4, String> {
        let e = self.get(rr).ok_or("missing relationship")?;
        let base = e.part("REPRESENTATION_RELATIONSHIP").ok_or("not a representation relationship")?;
        let (r1, r2) = (base.get(2).and_then(P::as_ref).ok_or("bad relationship")?, base.get(3).and_then(P::as_ref).ok_or("bad relationship")?);
        let op = e.part("REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION").and_then(|p| p.first()).and_then(P::as_ref).ok_or("no transformation")?;
        let ope = self.get(op).ok_or("missing transformation")?.clone();
        let scale_of = |me: &mut Self, r: u64| {
            let ctx = me.get(r).and_then(|x| x.params().get(2).and_then(P::as_ref));
            ctx.map_or(1.0, |c| me.context_scale(c))
        };
        let (s1, s2) = (scale_of(self, r1), scale_of(self, r2));
        if ope.name() == "ITEM_DEFINED_TRANSFORMATION" {
            let p = ope.params();
            let i1 = p.get(2).and_then(P::as_ref).ok_or("bad transformation")?;
            let i2 = p.get(3).and_then(P::as_ref).ok_or("bad transformation")?;
            let m1 = self.placement(i1, s1).ok_or("unreadable placement")?;
            let m2 = self.placement(i2, s2).ok_or("unreadable placement")?;
            let child_is_1 = child_reps.contains(&r1) || !child_reps.contains(&r2);
            Ok(if child_is_1 { m2 * m1.inverse_rigid() } else { m1 * m2.inverse_rigid() })
        } else {
            Err(format!("unsupported transformation {}", ope.name()))
        }
    }

    fn solid(&self, id: u64, scale: f64) -> Option<Solid> {
        let e = self.get(id)?;
        let (name, ps) = e.parts.first()?;
        let shells: Vec<u64> = match name.as_str() {
            "MANIFOLD_SOLID_BREP" => vec![ps.get(1)?.as_ref()?],
            "BREP_WITH_VOIDS" => {
                let mut v = vec![ps.get(1)?.as_ref()?];
                v.extend(ps.get(2).map(|l| l.as_list().iter().filter_map(P::as_ref).collect::<Vec<_>>()).unwrap_or_default());
                v
            }
            _ => return None,
        };
        let label = ps.first().and_then(P::as_str).unwrap_or("").to_string();
        let mut faces = Vec::new();
        for sh in shells {
            let Some(shell) = self.get(sh) else { continue };
            let face_ids: Vec<u64> = shell.params().get(1).map(|l| l.as_list().iter().filter_map(P::as_ref).collect()).unwrap_or_default();
            for f in face_ids {
                if let Some(face) = self.face(f, scale) {
                    faces.push(face);
                }
            }
        }
        Some(Solid { name: label, faces })
    }

    fn face(&self, id: u64, scale: f64) -> Option<Face> {
        let e = self.get(id)?;
        let ps = e.part("ADVANCED_FACE").or_else(|| e.part("FACE_SURFACE"))?;
        let bounds: Vec<u64> = ps.get(1)?.as_list().iter().filter_map(P::as_ref).collect();
        let surface_id = ps.get(2)?.as_ref()?;
        let same_sense = ps.get(3).and_then(P::as_bool).unwrap_or(true);
        let surface = self.surface(surface_id, scale);
        let mut edges = Vec::new();
        let mut vertices = Vec::new();
        let mut circles = Vec::new();
        let mut loops: Vec<Vec<V3>> = Vec::new();
        // Outer bound first.
        let mut ordered = bounds.clone();
        ordered.sort_by_key(|b| self.get(*b).map_or(1, |x| if x.is("FACE_OUTER_BOUND") { 0 } else { 1 }));
        for b in ordered {
            let Some(be) = self.get(b) else { continue };
            let bp = be.params();
            let Some(lp) = bp.get(1).and_then(P::as_ref).and_then(|l| self.get(l)) else { continue };
            let mut this_loop = Vec::new();
            if lp.is("VERTEX_LOOP") {
                if let Some(v) = lp.params().get(1).and_then(P::as_ref).and_then(|v| self.vertex(v, scale)) {
                    vertices.push(v);
                    this_loop.push(v);
                }
                loops.push(this_loop);
                continue;
            }
            let oes: Vec<u64> = lp.params().get(1).map(|l| l.as_list().iter().filter_map(P::as_ref).collect()).unwrap_or_default();
            for oe in oes {
                let Some(oee) = self.get(oe) else { continue };
                let op = oee.params();
                let Some(ec) = op.get(3).and_then(P::as_ref) else { continue };
                edges.push(ec);
                let Some(ece) = self.get(ec) else { continue };
                let ep = ece.params();
                // The loop runs from the edge's start when the oriented edge agrees with it.
                let forward = op.get(4).and_then(P::as_bool).unwrap_or(true);
                let vi = if forward { 1 } else { 2 };
                if let Some(v) = ep.get(vi).and_then(P::as_ref).and_then(|v| self.vertex(v, scale)) {
                    vertices.push(v);
                    this_loop.push(v);
                }
                if let Some(curve) = ep.get(3).and_then(P::as_ref).and_then(|c| self.get(c)) {
                    if curve.is("CIRCLE") {
                        let cp = curve.part("CIRCLE").unwrap_or(&[]);
                        if let (Some(pl), Some(r)) = (cp.get(1).and_then(P::as_ref).and_then(|p| self.placement(p, scale)), cp.get(2).and_then(P::as_num)) {
                            circles.push((pl.origin(), pl.col(2), r * scale));
                        }
                    }
                }
            }
            loops.push(this_loop);
        }
        Some(Face { surface, same_sense, edges, vertices, loops, circles })
    }

    fn vertex(&self, id: u64, scale: f64) -> Option<V3> {
        let e = self.get(id)?;
        self.point(e.part("VERTEX_POINT")?.get(1)?.as_ref()?, scale)
    }

    fn surface(&self, id: u64, scale: f64) -> Surface {
        let Some(e) = self.get(id) else { return Surface::Other("missing".into()) };
        let name = e.name().to_string();
        let ps = e.params();
        let pl = ps.get(1).and_then(P::as_ref).and_then(|p| self.placement(p, scale));
        let num = |i: usize| ps.get(i).and_then(P::as_num);
        match (name.as_str(), pl) {
            ("PLANE", Some(m)) => Surface::Plane { origin: m.origin(), normal: m.col(2) },
            ("CYLINDRICAL_SURFACE", Some(m)) => match (num(2), Axis::new(m.origin(), m.col(2))) {
                (Some(r), Some(axis)) => Surface::Cylinder { axis: Axis { origin: m.origin(), dir: axis.dir }, radius: r * scale },
                _ => Surface::Other(name),
            },
            ("CONICAL_SURFACE", Some(m)) => match (num(2), num(3), Axis::new(m.origin(), m.col(2))) {
                (Some(r), Some(a), Some(axis)) => Surface::Cone { axis: Axis { origin: m.origin(), dir: axis.dir }, apex_radius: r * scale, half_angle: a },
                _ => Surface::Other(name),
            },
            ("SPHERICAL_SURFACE", Some(m)) => num(2).map_or(Surface::Other(name.clone()), |r| Surface::Sphere { center: m.origin(), radius: r * scale }),
            ("TOROIDAL_SURFACE", Some(m)) => match (num(2), num(3), Axis::new(m.origin(), m.col(2))) {
                (Some(a), Some(b), Some(axis)) => Surface::Torus { axis: Axis { origin: m.origin(), dir: axis.dir }, major: a * scale, minor: b * scale },
                _ => Surface::Other(name),
            },
            _ => Surface::Other(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUBE_ASM: &str = include_str!("../tests/two_plates.stp");

    #[test]
    fn parses_entities_and_strings() {
        let t = "ISO-10303-21;\nDATA;\n#1=PRODUCT('A''b','\\X2\\00E9\\X0\\',$,(#2));\n#2=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.));\nENDSEC;";
        let e = parse_entities(t).unwrap();
        assert_eq!(e[&1].params()[0], P::Str("A'b".into()));
        assert_eq!(e[&1].params()[1], P::Str("é".into()));
        assert!(e[&2].is("SI_UNIT"));
    }

    #[test]
    fn extracts_one_part_that_reads_back_alone() {
        let f = StepFile::parse(CUBE_ASM.to_string()).unwrap();
        let m = f.model().unwrap();
        let root = &m.products[m.roots[0]];
        let plate = &m.products[root.children[0].product];
        let text = f.extract_product(plate.pd).unwrap();
        let alone = StepModel::parse(&text).unwrap();
        assert_eq!(alone.products.len(), 1);
        assert_eq!(alone.products[0].name, plate.name);
        assert_eq!(alone.products[0].solids, plate.solids);
    }

    #[test]
    fn reads_an_assembly_with_placements_and_faces() {
        let m = StepModel::parse(CUBE_ASM).unwrap();
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        assert_eq!(m.roots.len(), 1);
        let root = &m.products[m.roots[0]];
        assert_eq!(root.children.len(), 2);
        let plate = &m.products[root.children[0].product];
        assert_eq!(plate.solids.len(), 1);
        assert_eq!(plate.solids[0].faces.len(), 6);
        let inst = m.instances(m.roots[0]);
        let second = inst.iter().find(|(_, _, path)| path == &vec!["plate:2".to_string()]).unwrap();
        assert!(second.1.origin().dist(lcw_geom::v3(0.0, 0.0, 10.0)) < 1e-9, "{:?}", second.1.origin());
    }
}
