//! The interchange bundle: what an Inventor export carries into SolidCraft (and, later, into
//! Laser Cut Workbench).
//!
//! A bundle is a folder:
//!
//! ```text
//! bundle/
//!   bundle.json          this schema ("lcw-bundle/1")
//!   parts/<id>.stp       one STEP per part document, in the part's own frame
//!   flat/<id>.dxf        one flat pattern per sheet metal part (optional)
//! ```
//!
//! The schema is versioned and only ever grows: a reader ignores fields it does not know, and a
//! writer of a newer minor version keeps every field of the older ones. A change that breaks that
//! rule takes a new major version (`lcw-bundle/2`) and a reader for both.
//!
//! Units are millimetres and degrees. Placements follow `lcw_geom` (row-major, column vectors).
//! Constraint and joint geometry is in the coordinates of the assembly that owns it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use lcw_geom::{M4, V3};
use lcw_json::{obj, Json};

pub const SCHEMA: &str = "lcw-bundle/1";
pub const SCHEMA_MAJOR: u32 = 1;
pub const FILE_NAME: &str = "bundle.json";

#[derive(Debug, Clone, PartialEq)]
pub struct BundleError(pub String);

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BundleError {}

fn err<T>(m: impl Into<String>) -> Result<T, BundleError> {
    Err(BundleError(m.into()))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Source {
    /// "Autodesk Inventor", "STEP".
    pub app: String,
    pub app_version: String,
    /// The program that wrote the bundle and its version.
    pub exporter: String,
    pub exported_at: String,
    /// The file the export started from.
    pub root_file: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocKind {
    #[default]
    Part,
    Assembly,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mass {
    pub volume_mm3: Option<f64>,
    pub area_mm2: Option<f64>,
    pub bbox_min: Option<V3>,
    pub bbox_max: Option<V3>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BendDirection {
    #[default]
    Up,
    Down,
}

/// One bend of a flat pattern, as Inventor's flat bend results give it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlatBend {
    /// Centre line in flat-pattern coordinates (x, y on the flat's top face).
    pub start: [f64; 2],
    pub end: [f64; 2],
    /// Bend angle (0 flat, 90 a right angle), always positive.
    pub angle_deg: f64,
    pub direction: BendDirection,
    pub inner_radius: f64,
    /// A point on the bend's inner cylindrical face in the folded part (model coordinates).
    pub folded_point: Option<V3>,
    /// Inventor's own bend id, when it has one (for matching edits).
    pub id: Option<String>,
}

/// A closed loop of the flat outline: line and arc segments in flat coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum Seg2 {
    Line { a: [f64; 2], b: [f64; 2] },
    /// Counter-clockwise from `start` to `end` about `center` when `ccw`, else clockwise.
    Arc { center: [f64; 2], radius: f64, start: [f64; 2], end: [f64; 2], ccw: bool },
    Circle { center: [f64; 2], radius: f64 },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlatPattern {
    /// The flat DXF Inventor wrote, relative to the bundle folder.
    pub dxf: Option<String>,
    /// The outline and holes (outer loop first), when the exporter read them.
    pub loops: Vec<Vec<Seg2>>,
    pub bends: Vec<FlatBend>,
    /// Flat-pattern coordinates (top face at z = 0, sheet below it) to model coordinates for the
    /// base face's panel: where the flat would lie so that its base panel coincides with the
    /// folded part's.
    pub flat_to_model: Option<M4>,
    pub size: Option<[f64; 2]>,
}

/// A sheet metal part's rule and geometry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SheetMetal {
    pub thickness: f64,
    pub bend_radius: Option<f64>,
    pub k_factor: Option<f64>,
    /// "linear", "bend_table", "equation"…: only linear (K-factor) rules carry over exactly.
    pub unfold_method: Option<String>,
    pub style: Option<String>,
    pub relief_width: Option<f64>,
    pub relief_depth: Option<f64>,
    pub gap: Option<f64>,
    /// A point on the folded part's base (fixed) face and its outward normal.
    pub base_point: Option<V3>,
    pub base_normal: Option<V3>,
    /// Points on the folded part's bend faces (one per bend), model coordinates.
    pub bend_points: Vec<V3>,
    pub flat: Option<FlatPattern>,
}

/// A part derived from another one (Inventor Derive / Mirror Part).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Derived {
    pub source: String,
    pub mirrored: bool,
    pub plane_point: Option<V3>,
    pub plane_normal: Option<V3>,
}

/// A placement of a document inside an assembly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Occurrence {
    /// Unique within its assembly (Inventor's occurrence name, e.g. "Bracket:2").
    pub id: String,
    pub name: String,
    pub document: String,
    /// Placement in the owning assembly's frame.
    pub transform: M4,
    pub grounded: bool,
    pub suppressed: bool,
    pub visible: bool,
    /// The pattern this occurrence was made by, if any.
    pub pattern: Option<String>,
    /// Inventor "flexible" sub-assembly (its own constraints solve in the parent).
    pub flexible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntityKind {
    #[default]
    Plane,
    Axis,
    Point,
    Cylinder,
    Cone,
    Sphere,
    Edge,
    Other,
}

impl EntityKind {
    pub fn name(self) -> &'static str {
        match self {
            EntityKind::Plane => "plane",
            EntityKind::Axis => "axis",
            EntityKind::Point => "point",
            EntityKind::Cylinder => "cylinder",
            EntityKind::Cone => "cone",
            EntityKind::Sphere => "sphere",
            EntityKind::Edge => "edge",
            EntityKind::Other => "other",
        }
    }
    pub fn parse(s: &str) -> EntityKind {
        match s {
            "plane" | "face" | "workplane" => EntityKind::Plane,
            "axis" | "workaxis" | "line" => EntityKind::Axis,
            "point" | "workpoint" | "vertex" => EntityKind::Point,
            "cylinder" => EntityKind::Cylinder,
            "cone" => EntityKind::Cone,
            "sphere" => EntityKind::Sphere,
            "edge" | "circle" => EntityKind::Edge,
            _ => EntityKind::Other,
        }
    }
    /// Has an axis (round faces, work axes, round edges).
    pub fn is_axial(self) -> bool {
        matches!(self, EntityKind::Axis | EntityKind::Cylinder | EntityKind::Cone | EntityKind::Edge)
    }
}

/// One side of a constraint: a piece of geometry of one occurrence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntityRef {
    /// Occurrence ids from the owning assembly down to the part holding the geometry.
    pub path: Vec<String>,
    pub kind: EntityKind,
    /// A point on the entity (on the face, the axis, the edge).
    pub point: V3,
    /// Plane normal (outward for a face) or axis direction.
    pub dir: Option<V3>,
    pub radius: Option<f64>,
    /// A point on a round edge of the face (SolidCraft picks circles by their rim).
    pub rim_point: Option<V3>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Constraint {
    pub id: String,
    pub name: String,
    /// mate, flush, angle, insert, tangent, transitional, symmetry, rotation, translation…
    pub kind: String,
    pub a: EntityRef,
    pub b: EntityRef,
    pub offset: Option<f64>,
    pub angle_deg: Option<f64>,
    /// Inventor's solution type when it matters (opposed, aligned, directed, undirected).
    pub solution: Option<String>,
    pub suppressed: bool,
    /// Limits Inventor stores for the offset or angle.
    pub limits: Option<[f64; 2]>,
}

/// A joint origin: a frame on one occurrence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JointSide {
    pub path: Vec<String>,
    pub origin: V3,
    pub z: V3,
    pub x: Option<V3>,
}

/// An Inventor assembly joint (Rigid, Rotational, Slider, Cylindrical, Planar, Ball).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Joint {
    pub id: String,
    pub name: String,
    /// rigid, rotational, slider, cylindrical, planar, ball.
    pub kind: String,
    pub a: JointSide,
    pub b: JointSide,
    pub limits: Vec<[f64; 2]>,
    pub suppressed: bool,
}

/// A component pattern (Inventor rectangular, circular or feature pattern of occurrences).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pattern {
    pub id: String,
    pub name: String,
    pub kind: String,
    /// Seed first; one entry per element (the element's occurrence ids).
    pub elements: Vec<Vec<String>>,
    /// Exporter-specific parameters (counts, spacing, axis) kept as given.
    pub params: Json,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    pub id: String,
    pub kind: DocKind,
    pub name: String,
    /// The original file (e.g. C:\…\Bracket.ipt).
    pub file: String,
    pub part_number: Option<String>,
    pub description: Option<String>,
    pub material: Option<String>,
    /// STEP of this part in its own frame, relative to the bundle folder.
    pub step: Option<String>,
    pub mass: Mass,
    pub sheet_metal: Option<SheetMetal>,
    pub derived: Option<Derived>,
    /// Assembly contents (empty for parts).
    pub occurrences: Vec<Occurrence>,
    pub constraints: Vec<Constraint>,
    pub joints: Vec<Joint>,
    pub patterns: Vec<Pattern>,
    /// iProperties and anything else worth keeping.
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundle {
    pub schema: String,
    pub source: Source,
    /// The top assembly (or the single part).
    pub root: String,
    pub documents: Vec<Document>,
    /// Problems the exporter met (a part it could not export, a constraint it could not read).
    pub warnings: Vec<String>,
    /// The folder the bundle was read from (not serialised).
    pub folder: Option<PathBuf>,
}

impl Bundle {
    pub fn doc(&self, id: &str) -> Option<&Document> {
        self.documents.iter().find(|d| d.id == id)
    }
    pub fn root_doc(&self) -> Option<&Document> {
        self.doc(&self.root)
    }
    /// Resolve a bundle-relative path.
    pub fn path(&self, rel: &str) -> PathBuf {
        match &self.folder {
            Some(f) => f.join(rel),
            None => PathBuf::from(rel),
        }
    }

    /// Read `bundle.json` from a folder (or the file itself).
    pub fn read(path: &Path) -> Result<Bundle, BundleError> {
        let file = if path.is_dir() { path.join(FILE_NAME) } else { path.to_path_buf() };
        let text = std::fs::read_to_string(&file).map_err(|e| BundleError(format!("{}: {e}", file.display())))?;
        let json = Json::parse(&text).map_err(|e| BundleError(format!("{}: {e}", file.display())))?;
        let mut b = Bundle::from_json(&json)?;
        b.folder = file.parent().map(Path::to_path_buf);
        b.prune();
        Ok(b)
    }

    /// Drop what the exporter recorded but left out of the model, and say so in `warnings`:
    /// suppressed occurrences with no document, and constraints, joints and pattern elements that
    /// name an occurrence which is hidden, suppressed or whose file is missing. A real assembly
    /// nearly always has some; they are not a reason to refuse it.
    pub fn prune(&mut self) {
        let known: std::collections::BTreeSet<String> = self.documents.iter().map(|d| d.id.clone()).collect();
        let mut notes = Vec::new();
        for d in &mut self.documents {
            let before = d.occurrences.len();
            d.occurrences.retain(|o| known.contains(&o.document) || !(o.suppressed || o.document.is_empty()));
            let left_out = before - d.occurrences.len();
            if left_out > 0 {
                notes.push(format!("{}: {left_out} suppressed occurrence(s) left out", d.name));
            }
            let occ: std::collections::BTreeSet<String> = d.occurrences.iter().map(|o| o.id.clone()).collect();
            let absent = |path: &Vec<String>| path.first().is_some_and(|p| !occ.contains(p));
            let mut missing: BTreeMap<String, usize> = BTreeMap::new();
            let mut count = |path: &Vec<String>, missing: &mut BTreeMap<String, usize>| {
                if let Some(p) = path.first().filter(|p| !occ.contains(*p)) {
                    *missing.entry(p.clone()).or_default() += 1;
                }
            };
            for c in &d.constraints {
                count(&c.a.path, &mut missing);
                count(&c.b.path, &mut missing);
            }
            for j in &d.joints {
                count(&j.a.path, &mut missing);
                count(&j.b.path, &mut missing);
            }
            d.constraints.retain(|c| !absent(&c.a.path) && !absent(&c.b.path));
            d.joints.retain(|j| !absent(&j.a.path) && !absent(&j.b.path));
            for p in &mut d.patterns {
                p.elements.retain(|e| !absent(e));
            }
            for (name, n) in missing {
                notes.push(format!("{}: {n} constraint or joint reference(s) to `{name}`, which is not in the model (hidden, suppressed or missing), left out", d.name));
            }
        }
        self.warnings.extend(notes);
    }

    pub fn write(&self, folder: &Path) -> Result<(), BundleError> {
        std::fs::create_dir_all(folder).map_err(|e| BundleError(e.to_string()))?;
        std::fs::write(folder.join(FILE_NAME), self.to_json().to_pretty()).map_err(|e| BundleError(e.to_string()))
    }

    /// Check what later stages rely on: a known major schema, a root, unique ids, references
    /// that resolve, finite rigid placements.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        for d in &self.documents {
            if !ids.insert(d.id.as_str()) {
                problems.push(format!("document id `{}` is used twice", d.id));
            }
        }
        if self.doc(&self.root).is_none() {
            problems.push(format!("root `{}` is not a document", self.root));
        }
        for d in &self.documents {
            let mut occ_ids = std::collections::BTreeSet::new();
            for o in &d.occurrences {
                if !occ_ids.insert(o.id.as_str()) {
                    problems.push(format!("{}: occurrence `{}` is listed twice", d.name, o.id));
                }
                if self.doc(&o.document).is_none() {
                    problems.push(format!("{}: occurrence `{}` places unknown document `{}`", d.name, o.id, o.document));
                }
                if o.transform.orthonormal_error() > 1e-4 {
                    problems.push(format!("{}: occurrence `{}` has a scaled or sheared placement", d.name, o.id));
                }
            }
            for c in &d.constraints {
                for side in [&c.a, &c.b] {
                    if side.path.first().is_some_and(|p| !occ_ids.contains(p.as_str())) {
                        problems.push(format!("{}: constraint `{}` names unknown occurrence `{}`", d.name, c.name, side.path[0]));
                    }
                }
            }
        }
        problems
    }

    pub fn from_json(j: &Json) -> Result<Bundle, BundleError> {
        let schema = j.str_of("schema").unwrap_or_default().to_string();
        let major = schema.strip_prefix("lcw-bundle/").and_then(|v| v.split('.').next()).and_then(|v| v.parse::<u32>().ok());
        match major {
            Some(SCHEMA_MAJOR) => {}
            Some(m) if m > SCHEMA_MAJOR => return err(format!("bundle schema `{schema}` is newer than this converter reads ({SCHEMA}); update inv2sc")),
            _ => return err(format!("not a bundle: schema is `{schema}`, expected `{SCHEMA}`")),
        }
        let src = j.get("source").cloned().unwrap_or_default();
        let source = Source {
            app: s(&src, "app"),
            app_version: s(&src, "app_version"),
            exporter: s(&src, "exporter"),
            exported_at: s(&src, "exported_at"),
            root_file: s(&src, "root_file"),
        };
        let documents = j.arr_of("documents").iter().map(doc_from).collect::<Result<Vec<_>, _>>()?;
        Ok(Bundle {
            schema,
            source,
            root: s(j, "root"),
            documents,
            warnings: j.arr_of("warnings").iter().filter_map(|w| w.as_str().map(str::to_string)).collect(),
            folder: None,
        })
    }

    pub fn to_json(&self) -> Json {
        obj! {
            "schema" => SCHEMA,
            "source" => obj!{
                "app" => &self.source.app,
                "app_version" => &self.source.app_version,
                "exporter" => &self.source.exporter,
                "exported_at" => &self.source.exported_at,
                "root_file" => &self.source.root_file,
            },
            "root" => &self.root,
            "documents" => Json::Arr(self.documents.iter().map(doc_to).collect()),
            "warnings" => self.warnings.clone(),
        }
    }
}

fn s(j: &Json, k: &str) -> String {
    j.str_of(k).unwrap_or_default().to_string()
}
fn os(j: &Json, k: &str) -> Option<String> {
    j.str_of(k).filter(|v| !v.is_empty()).map(str::to_string)
}
fn v(j: &Json, k: &str) -> Option<V3> {
    j.vec_of::<3>(k).map(V3::from_array)
}
fn vj(p: V3) -> Json {
    Json::from(p.to_array())
}
fn ovj(p: Option<V3>) -> Json {
    p.map_or(Json::Null, vj)
}
fn m4(j: &Json, k: &str) -> Result<Option<M4>, BundleError> {
    let Some(raw) = j.get(k).filter(|x| !x.is_null()) else { return Ok(None) };
    let flat: Vec<f64> = match raw.as_array() {
        Some(items) if items.iter().all(|x| x.as_array().is_some()) => {
            items.iter().flat_map(|r| r.as_array().unwrap_or(&[]).iter().filter_map(Json::as_f64)).collect()
        }
        Some(items) => items.iter().filter_map(Json::as_f64).collect(),
        None => Vec::new(),
    };
    M4::from_slice(&flat).map(Some).ok_or_else(|| BundleError(format!("`{k}` must be 12 or 16 finite numbers")))
}
fn m4j(m: &M4) -> Json {
    Json::from(m.to_vec16())
}
fn path_of(j: &Json, k: &str) -> Vec<String> {
    match j.get(k) {
        Some(Json::Str(one)) => vec![one.clone()],
        Some(Json::Arr(items)) => items.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}
fn pair(j: &Json) -> Option<[f64; 2]> {
    lcw_json::to_array::<2>(j)
}

fn doc_from(j: &Json) -> Result<Document, BundleError> {
    let id = s(j, "id");
    if id.is_empty() {
        return err("a document has no `id`");
    }
    let kind = match j.str_of("kind") {
        Some("assembly") => DocKind::Assembly,
        _ => DocKind::Part,
    };
    let mass_j = j.get("mass").cloned().unwrap_or_default();
    let mass = Mass {
        // The STEP holds the visible solid bodies only, so when the exporter says what those add up
        // to, that is the volume to compare against; Inventor's own figure counts hidden bodies too.
        volume_mm3: mass_j.f64_of("visible_volume_mm3").or(mass_j.f64_of("volume_mm3")),
        area_mm2: mass_j.f64_of("area_mm2"),
        bbox_min: v(&mass_j, "bbox_min"),
        bbox_max: v(&mass_j, "bbox_max"),
    };
    let mut occurrences = Vec::new();
    for o in j.arr_of("occurrences") {
        occurrences.push(Occurrence {
            id: s(o, "id"),
            name: os(o, "name").unwrap_or_else(|| s(o, "id")),
            document: s(o, "document"),
            transform: m4(o, "transform")?.unwrap_or_default(),
            grounded: o.bool_of("grounded").unwrap_or(false),
            suppressed: o.bool_of("suppressed").unwrap_or(false),
            visible: o.bool_of("visible").unwrap_or(true),
            pattern: os(o, "pattern"),
            flexible: o.bool_of("flexible").unwrap_or(false),
        });
    }
    let constraints = j
        .arr_of("constraints")
        .iter()
        .map(|c| Constraint {
            id: s(c, "id"),
            name: os(c, "name").unwrap_or_else(|| s(c, "id")),
            kind: s(c, "kind").to_ascii_lowercase(),
            a: entity_from(c.get("a").unwrap_or(&Json::Null)),
            b: entity_from(c.get("b").unwrap_or(&Json::Null)),
            offset: c.f64_of("offset"),
            angle_deg: c.f64_of("angle_deg"),
            solution: os(c, "solution"),
            suppressed: c.bool_of("suppressed").unwrap_or(false),
            limits: c.get("limits").and_then(pair),
        })
        .collect();
    let joints = j
        .arr_of("joints")
        .iter()
        .map(|x| Joint {
            id: s(x, "id"),
            name: os(x, "name").unwrap_or_else(|| s(x, "id")),
            kind: s(x, "kind").to_ascii_lowercase(),
            a: side_from(x.get("a").unwrap_or(&Json::Null)),
            b: side_from(x.get("b").unwrap_or(&Json::Null)),
            limits: x.arr_of("limits").iter().filter_map(pair).collect(),
            suppressed: x.bool_of("suppressed").unwrap_or(false),
        })
        .collect();
    let patterns = j
        .arr_of("patterns")
        .iter()
        .map(|p| Pattern {
            id: s(p, "id"),
            name: os(p, "name").unwrap_or_else(|| s(p, "id")),
            kind: s(p, "kind").to_ascii_lowercase(),
            elements: p.arr_of("elements").iter().map(|e| path_of(&obj! {"e" => e.clone()}, "e")).collect(),
            params: p.get("params").cloned().unwrap_or_default(),
        })
        .collect();
    let properties = j
        .get("properties")
        .and_then(Json::as_object)
        .map(|items| items.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
        .unwrap_or_default();
    let derived = j.get("derived").filter(|d| !d.is_null()).map(|d| Derived {
        source: s(d, "source"),
        mirrored: d.bool_of("mirrored").unwrap_or(false),
        plane_point: v(d, "plane_point"),
        plane_normal: v(d, "plane_normal"),
    });
    let sheet_metal = match j.get("sheet_metal").filter(|d| !d.is_null()) {
        Some(sm) => Some(sheet_from(sm)?),
        None => None,
    };
    Ok(Document {
        id,
        kind,
        name: s(j, "name"),
        file: s(j, "file"),
        part_number: os(j, "part_number"),
        description: os(j, "description"),
        material: os(j, "material"),
        step: os(j, "step"),
        mass,
        sheet_metal,
        derived,
        occurrences,
        constraints,
        joints,
        patterns,
        properties,
    })
}

fn entity_from(j: &Json) -> EntityRef {
    EntityRef {
        path: path_of(j, "path"),
        kind: EntityKind::parse(j.str_of("kind").unwrap_or("other")),
        point: v(j, "point").unwrap_or_default(),
        dir: v(j, "dir"),
        radius: j.f64_of("radius"),
        rim_point: v(j, "rim_point"),
    }
}

fn entity_to(e: &EntityRef) -> Json {
    obj! {
        "path" => e.path.clone(),
        "kind" => e.kind.name(),
        "point" => vj(e.point),
        "dir" => ovj(e.dir),
        "radius" => e.radius,
        "rim_point" => ovj(e.rim_point),
    }
}

fn side_from(j: &Json) -> JointSide {
    JointSide { path: path_of(j, "path"), origin: v(j, "origin").unwrap_or_default(), z: v(j, "z").unwrap_or(V3::Z), x: v(j, "x") }
}

fn side_to(s: &JointSide) -> Json {
    obj! {"path" => s.path.clone(), "origin" => vj(s.origin), "z" => vj(s.z), "x" => ovj(s.x)}
}

fn seg_from(j: &Json) -> Option<Seg2> {
    match j.str_of("type")? {
        "line" => Some(Seg2::Line { a: pair(j.get("a")?)?, b: pair(j.get("b")?)? }),
        "arc" => Some(Seg2::Arc {
            center: pair(j.get("center")?)?,
            radius: j.f64_of("radius")?,
            start: pair(j.get("start")?)?,
            end: pair(j.get("end")?)?,
            ccw: j.bool_of("ccw").unwrap_or(true),
        }),
        "circle" => Some(Seg2::Circle { center: pair(j.get("center")?)?, radius: j.f64_of("radius")? }),
        _ => None,
    }
}

fn seg_to(s: &Seg2) -> Json {
    match s {
        Seg2::Line { a, b } => obj! {"type" => "line", "a" => *a, "b" => *b},
        Seg2::Arc { center, radius, start, end, ccw } => {
            obj! {"type" => "arc", "center" => *center, "radius" => *radius, "start" => *start, "end" => *end, "ccw" => *ccw}
        }
        Seg2::Circle { center, radius } => obj! {"type" => "circle", "center" => *center, "radius" => *radius},
    }
}

fn sheet_from(j: &Json) -> Result<SheetMetal, BundleError> {
    let thickness = j.f64_of("thickness").filter(|t| *t > 0.0).ok_or_else(|| BundleError("sheet_metal.thickness must be positive".into()))?;
    let flat = match j.get("flat").filter(|f| !f.is_null()) {
        None => None,
        Some(f) => Some(FlatPattern {
            dxf: os(f, "dxf"),
            loops: f.arr_of("loops").iter().map(|l| l.as_array().unwrap_or(&[]).iter().filter_map(seg_from).collect()).collect(),
            bends: f
                .arr_of("bends")
                .iter()
                .filter_map(|b| {
                    Some(FlatBend {
                        start: pair(b.get("start")?)?,
                        end: pair(b.get("end")?)?,
                        angle_deg: b.f64_of("angle_deg")?.abs(),
                        direction: if b.str_of("direction") == Some("down") { BendDirection::Down } else { BendDirection::Up },
                        inner_radius: b.f64_of("inner_radius").unwrap_or(0.0),
                        folded_point: v(b, "folded_point"),
                        id: os(b, "id"),
                    })
                })
                .collect(),
            flat_to_model: m4(f, "flat_to_model")?,
            size: f.get("size").and_then(pair),
        }),
    };
    Ok(SheetMetal {
        thickness,
        bend_radius: j.f64_of("bend_radius"),
        k_factor: j.f64_of("k_factor"),
        unfold_method: os(j, "unfold_method"),
        style: os(j, "style"),
        relief_width: j.f64_of("relief_width"),
        relief_depth: j.f64_of("relief_depth"),
        gap: j.f64_of("gap"),
        base_point: v(j, "base_point"),
        base_normal: v(j, "base_normal"),
        bend_points: j.arr_of("bend_points").iter().filter_map(|p| lcw_json::to_array::<3>(p).map(V3::from_array)).collect(),
        flat,
    })
}

fn sheet_to(sm: &SheetMetal) -> Json {
    let flat = sm.flat.as_ref().map_or(Json::Null, |f| {
        obj! {
            "dxf" => f.dxf.clone(),
            "loops" => Json::Arr(f.loops.iter().map(|l| Json::Arr(l.iter().map(seg_to).collect())).collect()),
            "bends" => Json::Arr(f.bends.iter().map(|b| obj!{
                "start" => b.start, "end" => b.end, "angle_deg" => b.angle_deg,
                "direction" => if b.direction == BendDirection::Down { "down" } else { "up" },
                "inner_radius" => b.inner_radius, "folded_point" => ovj(b.folded_point), "id" => b.id.clone(),
            }).collect()),
            "flat_to_model" => f.flat_to_model.as_ref().map_or(Json::Null, m4j),
            "size" => f.size.map_or(Json::Null, Json::from),
        }
    });
    obj! {
        "thickness" => sm.thickness,
        "bend_radius" => sm.bend_radius,
        "k_factor" => sm.k_factor,
        "unfold_method" => sm.unfold_method.clone(),
        "style" => sm.style.clone(),
        "relief_width" => sm.relief_width,
        "relief_depth" => sm.relief_depth,
        "gap" => sm.gap,
        "base_point" => ovj(sm.base_point),
        "base_normal" => ovj(sm.base_normal),
        "bend_points" => Json::Arr(sm.bend_points.iter().map(|p| vj(*p)).collect()),
        "flat" => flat,
    }
}

fn doc_to(d: &Document) -> Json {
    let mut j = obj! {
        "id" => &d.id,
        "kind" => if d.kind == DocKind::Assembly { "assembly" } else { "part" },
        "name" => &d.name,
        "file" => &d.file,
        "part_number" => d.part_number.clone(),
        "description" => d.description.clone(),
        "material" => d.material.clone(),
        "step" => d.step.clone(),
        "mass" => obj!{
            "volume_mm3" => d.mass.volume_mm3, "area_mm2" => d.mass.area_mm2,
            "bbox_min" => ovj(d.mass.bbox_min), "bbox_max" => ovj(d.mass.bbox_max),
        },
        "sheet_metal" => d.sheet_metal.as_ref().map_or(Json::Null, sheet_to),
        "derived" => d.derived.as_ref().map_or(Json::Null, |x| obj!{
            "source" => &x.source, "mirrored" => x.mirrored, "plane_point" => ovj(x.plane_point), "plane_normal" => ovj(x.plane_normal),
        }),
    };
    if d.kind == DocKind::Assembly {
        j.set(
            "occurrences",
            Json::Arr(
                d.occurrences
                    .iter()
                    .map(|o| {
                        obj! {
                            "id" => &o.id, "name" => &o.name, "document" => &o.document, "transform" => m4j(&o.transform),
                            "grounded" => o.grounded, "suppressed" => o.suppressed, "visible" => o.visible,
                            "pattern" => o.pattern.clone(), "flexible" => o.flexible,
                        }
                    })
                    .collect(),
            ),
        );
        j.set(
            "constraints",
            Json::Arr(
                d.constraints
                    .iter()
                    .map(|c| {
                        obj! {
                            "id" => &c.id, "name" => &c.name, "kind" => &c.kind, "a" => entity_to(&c.a), "b" => entity_to(&c.b),
                            "offset" => c.offset, "angle_deg" => c.angle_deg, "solution" => c.solution.clone(),
                            "suppressed" => c.suppressed, "limits" => c.limits.map_or(Json::Null, Json::from),
                        }
                    })
                    .collect(),
            ),
        );
        j.set(
            "joints",
            Json::Arr(
                d.joints
                    .iter()
                    .map(|x| {
                        obj! {
                            "id" => &x.id, "name" => &x.name, "kind" => &x.kind, "a" => side_to(&x.a), "b" => side_to(&x.b),
                            "limits" => Json::Arr(x.limits.iter().map(|l| Json::from(*l)).collect()), "suppressed" => x.suppressed,
                        }
                    })
                    .collect(),
            ),
        );
        j.set(
            "patterns",
            Json::Arr(
                d.patterns
                    .iter()
                    .map(|p| {
                        obj! {
                            "id" => &p.id, "name" => &p.name, "kind" => &p.kind,
                            "elements" => Json::Arr(p.elements.iter().map(|e| Json::from(e.clone())).collect()),
                            "params" => p.params.clone(),
                        }
                    })
                    .collect(),
            ),
        );
    }
    let mut props = Json::obj();
    for (k, val) in &d.properties {
        props.set(k, val.as_str());
    }
    j.set("properties", props);
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bundle {
        let part = Document {
            id: "p1".into(),
            name: "Bracket".into(),
            step: Some("parts/p1.stp".into()),
            sheet_metal: Some(SheetMetal {
                thickness: 2.0,
                bend_radius: Some(2.0),
                k_factor: Some(0.44),
                flat: Some(FlatPattern {
                    loops: vec![vec![Seg2::Line { a: [0.0, 0.0], b: [10.0, 0.0] }, Seg2::Circle { center: [5.0, 5.0], radius: 1.5 }]],
                    bends: vec![FlatBend { start: [0.0, 5.0], end: [10.0, 5.0], angle_deg: 90.0, direction: BendDirection::Down, inner_radius: 2.0, ..Default::default() }],
                    flat_to_model: Some(M4::translation(V3::new_for_test())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let asm = Document {
            id: "a1".into(),
            kind: DocKind::Assembly,
            name: "Top".into(),
            occurrences: vec![
                Occurrence { id: "Bracket:1".into(), name: "Bracket:1".into(), document: "p1".into(), grounded: true, visible: true, ..Default::default() },
                Occurrence { id: "Bracket:2".into(), name: "Bracket:2".into(), document: "p1".into(), transform: M4::translation(lcw_geom::v3(50.0, 0.0, 0.0)), visible: true, ..Default::default() },
            ],
            constraints: vec![Constraint {
                id: "c1".into(),
                name: "Mate:1".into(),
                kind: "mate".into(),
                a: EntityRef { path: vec!["Bracket:1".into()], kind: EntityKind::Plane, point: lcw_geom::v3(1.0, 2.0, 3.0), dir: Some(V3::Z), ..Default::default() },
                b: EntityRef { path: vec!["Bracket:2".into()], kind: EntityKind::Plane, point: lcw_geom::v3(51.0, 2.0, 3.0), dir: Some(-V3::Z), ..Default::default() },
                offset: Some(0.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        Bundle { schema: SCHEMA.into(), root: "a1".into(), documents: vec![asm, part], ..Default::default() }
    }

    trait ForTest {
        fn new_for_test() -> V3;
    }
    impl ForTest for V3 {
        fn new_for_test() -> V3 {
            lcw_geom::v3(1.0, 2.0, 3.0)
        }
    }

    #[test]
    fn round_trips_through_json() {
        let b = sample();
        let back = Bundle::from_json(&Json::parse(&b.to_json().to_pretty()).unwrap()).unwrap();
        assert_eq!(back.documents, b.documents);
        assert!(back.validate().is_empty(), "{:?}", back.validate());
    }

    #[test]
    fn refuses_newer_major_and_foreign_files() {
        let e = Bundle::from_json(&obj! {"schema" => "lcw-bundle/2"}).unwrap_err();
        assert!(e.0.contains("newer"));
        assert!(Bundle::from_json(&obj! {"schema" => "other"}).is_err());
        assert!(Bundle::from_json(&obj! {"schema" => "lcw-bundle/1.3", "root" => "x"}).is_ok(), "minor versions read");
    }

    #[test]
    fn validation_finds_dangling_references() {
        let mut b = sample();
        b.documents[0].occurrences[1].document = "missing".into();
        assert!(b.validate().iter().any(|p| p.contains("unknown document")));
    }
}
