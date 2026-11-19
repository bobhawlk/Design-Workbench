//! inv2sc: Inventor (or STEP) assemblies into clean, editable SolidCraft assemblies.

mod report;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use lcw_analyze::model::Model;
use lcw_analyze::{analyze, Analysis, Merge, Options};
use lcw_json::Json;
use lcw_solidcraft::emit::{assembly_plan, part_steps, BuildPlan, Options as EmitOptions, LIMITS_RECORD};
use lcw_solidcraft::mcp::{Launch, McpClient, Transport};
use lcw_solidcraft::profile::{Profile, DEFAULT_TIMEOUT};
use lcw_solidcraft::progress::ProgressSink;
use lcw_solidcraft::run::{DesignResult, Runner};
use lcw_solidcraft::steps::{designs_from_json, limits_from_json, relative_path, DesignSteps};

const USAGE: &str = "inv2sc — Inventor assemblies into editable SolidCraft assemblies

USAGE:
  inv2sc convert <bundle-folder | bundle.json | assembly.stp> [options]
  inv2sc analyze <bundle-folder | bundle.json | assembly.stp> [options]   (no SolidCraft needed)
  inv2sc watch <folder> [options]       convert each bundle that appears in <folder>
  inv2sc run <build-steps.json> [options]   build from a steps file written earlier
  inv2sc limits <folder convert wrote> [--redo] [options]
                find each joint's travel limits in a built assembly (the stage convert runs
                with `convert --limits`; after a plain `convert`, or to take up a stage that was
                cut short: joints already in limits-found.jsonl are not searched again, except
                those not found in time; --redo searches them all). Writes limits-report.md.
  inv2sc probe [--solidcraft-cli PATH] [--connect HOST:PORT] [--profile FILE]
  inv2sc verify <bundle-folder | bundle.json> [--built DIR] [--assembly FILE] [options]
                check a finished build against the bundle: parts, placements, names, overall
                box, interference. Writes verification-report.md and verification.json (to
                --out, default the built folder); exits 1 when a check fails.
  inv2sc version

OPTIONS:
  --out DIR               where the SolidCraft designs go (default: <input>/SolidCraft)
  --solidcraft-cli PATH   solidcraft-cli executable (default: $SOLIDCRAFT_CLI, PATH, then the
                          standard install folder)
  --connect HOST:PORT     build in the running SolidCraft app (start it with --control PORT, or
                          turn on Preferences › AI › control channel) instead of headless
  --profile FILE          a command profile to use instead of the built-in one
  --dry-run               write the analysis, report and build steps, but don't run SolidCraft
  --merge MODE            identical parts with different files: report | same-name (default) | all
  --no-sheet-metal        bring every part in as a plain solid
  --no-infer              don't infer joints for unconstrained parts (they're held rigid)
  --limits                search each joint's travel for its limits after the assembly is
                          built (off by default: `inv2sc limits` can do it later; a limit that
                          is too tight stops a lever, none leaves it free). --no-limits is
                          accepted and does nothing (it is the default)
  --inventor-constraints  also create SolidCraft constraints from Inventor constraints whose
                          geometry lands on the parts (off by default: parts are joined by
                          their geometry, and Inventor's constraints are evidence only)
  --up y|z                which axis is up in the source (default: y for an Inventor bundle,
                          z for a plain STEP). With y the assembly is stood upright: one quarter
                          turn about X on the root, so Inventor's up is SolidCraft's +Z and its
                          front view is SolidCraft's front view. The turn is written to
                          orientation.json, which verify reads
  --keep-axes             keep the source's axes as they are (the same as --up z)
  --no-patterns           don't make pattern spacing a parameter
  --max-thickness MM      thickest plate treated as sheet metal (default 6)
  --path-map FROM=TO      write paths for another machine: build steps made here name
                          FROM... as TO... (e.g. /tmp/out=Z:\\exports\\console), for running the
                          steps with Run-BuildSteps.ps1 where SolidCraft is
  --relative-paths        write paths in build-steps.json relative to its folder
                          (${steps_dir}/parts/...), resolved when the steps are run, so the
                          output folder can be moved (inv2sc run only; Run-BuildSteps.ps1
                          doesn't read these yet)
  --timeout SECS          the least time any one SolidCraft call may take before it is given up
                          (default: the profile's, 300 s, longer for known slow commands). A call
                          that times out may still be running, so its session is ended and the
                          next design starts in a new one
  --batch N               send up to N independent steps to SolidCraft in one `batch` call
                          (default 1: one at a time)
  --built DIR             verify: the folder convert wrote (default: <bundle>/SolidCraft)
  --assembly FILE         verify: the assembly design (default: <built>/<top assembly name>.sc)
  --tol-mm MM             verify: position tolerance (default 0.05, or 1e-4 of the part's size)
  --tol-deg DEG           verify: frame angle tolerance (default 0.05)
  --clash-budget SECS     verify: time allowed for the interference check (default 600)
  --no-clash              verify: skip the interference check
  --root-turn y-up|none   verify: the turn convert put on the root (default: what the built
                          folder's orientation.json says, else none)
  --quiet                 no progress lines (progress.json and build-log.jsonl are still written)
  --verbose               print every step and its outcome
";

struct Args {
    cmd: String,
    input: Option<PathBuf>,
    out: Option<PathBuf>,
    cli: Option<String>,
    connect: Option<String>,
    profile: Option<PathBuf>,
    dry_run: bool,
    merge: Merge,
    sheet_metal: bool,
    infer: bool,
    limits: bool,
    inventor_constraints: bool,
    patterns: bool,
    max_thickness: f64,
    verbose: bool,
    path_map: Option<(String, String)>,
    relative_paths: bool,
    timeout: Option<Duration>,
    batch: usize,
    quiet: bool,
    built: Option<PathBuf>,
    assembly: Option<PathBuf>,
    tol_mm: Option<f64>,
    tol_deg: Option<f64>,
    clash_budget: Option<f64>,
    no_clash: bool,
    redo: bool,
    up: Option<String>,
    root_turn: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(|| USAGE.to_string())?;
    let mut a = Args {
        cmd,
        input: None,
        out: None,
        cli: None,
        connect: None,
        profile: None,
        dry_run: false,
        merge: Merge::SameName,
        sheet_metal: true,
        infer: true,
        limits: false,
        inventor_constraints: false,
        patterns: true,
        max_thickness: 6.0,
        verbose: false,
        path_map: None,
        relative_paths: false,
        timeout: None,
        batch: 1,
        quiet: false,
        built: None,
        assembly: None,
        tol_mm: None,
        tol_deg: None,
        clash_budget: None,
        no_clash: false,
        redo: false,
        up: None,
        root_turn: None,
    };
    while let Some(x) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match x.as_str() {
            "--out" => a.out = Some(PathBuf::from(value("--out")?)),
            "--solidcraft-cli" => a.cli = Some(value("--solidcraft-cli")?),
            "--connect" => a.connect = Some(value("--connect")?),
            "--profile" => a.profile = Some(PathBuf::from(value("--profile")?)),
            "--dry-run" => a.dry_run = true,
            "--merge" => {
                a.merge = match value("--merge")?.as_str() {
                    "report" => Merge::Report,
                    "same-name" => Merge::SameName,
                    "all" => Merge::All,
                    m => return Err(format!("--merge takes report, same-name or all, not `{m}`")),
                }
            }
            "--no-sheet-metal" => a.sheet_metal = false,
            "--no-infer" => a.infer = false,
            "--no-limits" => a.limits = false,
            "--limits" => a.limits = true,
            "--inventor-constraints" => a.inventor_constraints = true,
            "--no-patterns" => a.patterns = false,
            "--max-thickness" => a.max_thickness = value("--max-thickness")?.parse().map_err(|_| "--max-thickness takes a number of mm")?,
            "--verbose" | "-v" => a.verbose = true,
            "--quiet" | "-q" => a.quiet = true,
            "--relative-paths" => a.relative_paths = true,
            "--built" => a.built = Some(PathBuf::from(value("--built")?)),
            "--assembly" => a.assembly = Some(PathBuf::from(value("--assembly")?)),
            "--tol-mm" => a.tol_mm = Some(value("--tol-mm")?.parse().map_err(|_| "--tol-mm takes a number of mm")?),
            "--tol-deg" => a.tol_deg = Some(value("--tol-deg")?.parse().map_err(|_| "--tol-deg takes a number of degrees")?),
            "--clash-budget" => a.clash_budget = Some(value("--clash-budget")?.parse().map_err(|_| "--clash-budget takes a number of seconds")?),
            "--no-clash" => a.no_clash = true,
            "--redo" => a.redo = true,
            "--keep-axes" => a.up = Some("z".into()),
            "--up" => {
                let v = value("--up")?.to_ascii_lowercase();
                if v != "y" && v != "z" {
                    return Err(format!("--up takes y or z, not `{v}`"));
                }
                a.up = Some(v);
            }
            "--root-turn" => {
                let v = value("--root-turn")?.to_ascii_lowercase();
                if v != "y-up" && v != "none" {
                    return Err(format!("--root-turn takes y-up or none, not `{v}`"));
                }
                a.root_turn = Some(v);
            }
            "--timeout" => {
                let secs: f64 = value("--timeout")?.parse().map_err(|_| "--timeout takes a number of seconds")?;
                if !(secs.is_finite() && secs > 0.0) {
                    return Err("--timeout takes a number of seconds above 0".into());
                }
                a.timeout = Some(Duration::from_secs_f64(secs));
            }
            "--batch" => a.batch = value("--batch")?.parse().map_err(|_| "--batch takes a number of steps")?,
            "--path-map" => {
                let v = value("--path-map")?;
                let (from, to) = v.split_once('=').ok_or("--path-map takes FROM=TO")?;
                a.path_map = Some((from.to_string(), to.to_string()));
            }
            "-h" | "--help" => return Err(USAGE.to_string()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            s => {
                if a.input.is_some() {
                    return Err(format!("unexpected argument {s}"));
                }
                a.input = Some(PathBuf::from(s));
            }
        }
    }
    if a.relative_paths && a.path_map.is_some() {
        return Err("--relative-paths and --path-map can't be used together".into());
    }
    Ok(a)
}

fn find_cli(explicit: &Option<String>) -> String {
    if let Some(c) = explicit {
        return c.clone();
    }
    if let Ok(c) = std::env::var("SOLIDCRAFT_CLI") {
        if !c.is_empty() {
            return c;
        }
    }
    let exe = if cfg!(windows) { "solidcraft-cli.exe" } else { "solidcraft-cli" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(exe);
            if p.is_file() {
                return p.display().to_string();
            }
        }
    }
    let standard: &[&str] = if cfg!(windows) {
        &[r"C:\Program Files\SolidCraft\solidcraft-cli.exe", r"C:\Program Files\SolidCraft\bin\solidcraft-cli.exe"]
    } else if cfg!(target_os = "macos") {
        &["/Applications/SolidCraft.app/Contents/MacOS/solidcraft-cli"]
    } else {
        &["/usr/bin/solidcraft-cli", "/usr/local/bin/solidcraft-cli"]
    };
    standard.iter().find(|p| Path::new(p).is_file()).map(|p| p.to_string()).unwrap_or_else(|| exe.to_string())
}

fn load_profile(a: &Args) -> Result<Profile, String> {
    match &a.profile {
        Some(p) => Profile::parse(&std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?),
        None => Ok(Profile::builtin()),
    }
}

fn load_model(input: &Path) -> Result<Model, String> {
    if is_bundle(input) {
        let b = lcw_bundle::Bundle::read(input).map_err(|e| e.to_string())?;
        Model::from_bundle(&b)
    } else {
        Model::from_step(input)
    }
}

fn is_bundle(input: &Path) -> bool {
    input.is_dir() || input.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// The file in a built folder that says which turn convert put on the root (verify reads it).
const ORIENTATION_FILE: &str = "orientation.json";

/// The model, stood upright when its source has Y up (an Inventor bundle unless `--up z` or
/// `--keep-axes`; a plain STEP only with `--up y`). Returns the turn put on the root, if any.
fn load_upright(a: &Args, input: &Path) -> Result<(Model, Option<lcw_geom::M4>), String> {
    let mut model = load_model(input)?;
    let up = a.up.clone().unwrap_or_else(|| if is_bundle(input) { "y".into() } else { "z".into() });
    if up == "y" {
        let r = lcw_analyze::model::Y_UP_TO_Z_UP;
        model.turn_root(&r);
        return Ok((model, Some(r)));
    }
    Ok((model, None))
}

fn orientation_json(turn: Option<&lcw_geom::M4>, source_app: &str) -> Json {
    let m = turn.copied().unwrap_or(lcw_geom::M4::IDENTITY);
    let rows: Vec<f64> = m.0.iter().flatten().copied().collect();
    lcw_json::obj! {
        "schema" => "inv2sc-orientation/1",
        "about" => "The turn convert put on the assembly's root, as a row-major 4x4 matrix applied to the source's coordinates (column vectors): SolidCraft's world = root_turn x the source's world.",
        "source_app" => source_app,
        "source_up" => if turn.is_some() { "y" } else { "z" },
        "root_turn" => rows,
        "name" => if turn.is_some() { "y-up" } else { "none" },
    }
}

fn analysis_options(a: &Args) -> Options {
    let mut o = Options { merge: a.merge, ..Options::default() };
    o.sheet.max_thickness = a.max_thickness;
    o.joints.infer = a.infer;
    o.joints.inventor_constraints = a.inventor_constraints;
    o
}

fn spawn(a: &Args, allow: &[&Path]) -> Result<McpClient, String> {
    let cli = find_cli(&a.cli);
    let mut args = vec!["mcp".to_string()];
    match &a.connect {
        Some(addr) => {
            args.push("--connect".into());
            args.push(addr.clone());
        }
        None => args.push("--headless".into()),
    }
    for p in allow {
        args.push("--allow-path".into());
        args.push(p.display().to_string());
    }
    McpClient::start(Launch::new(&cli, &args), a.timeout.unwrap_or(DEFAULT_TIMEOUT))
}

/// Build `designs` in SolidCraft (the last is the assembly when `assembly_last`), with progress
/// on stderr, `<dir>/progress.json` and `<dir>/build-log.jsonl`. Returns the results, the probe
/// report and the session events.
/// Designs built, the probe's report on the profile, and what happened to the session.
type Built = (Vec<DesignResult>, Vec<String>, Vec<String>);

fn build(a: &Args, dir: &Path, steps_dir: &Path, designs: &[&DesignSteps], assembly_last: bool) -> Result<Built, String> {
    build_with(a, dir, steps_dir, designs, assembly_last, BTreeMap::new())
}

fn build_with(a: &Args, dir: &Path, steps_dir: &Path, designs: &[&DesignSteps], assembly_last: bool, preset: BTreeMap<String, Json>) -> Result<Built, String> {
    let mut sink = ProgressSink::new(Some(dir.join("progress.json")), Some(dir.join("build-log.jsonl")))?;
    sink.print = !a.quiet;
    sink.verbose = a.verbose;
    sink.design_total = designs.len();
    sink.assembly_index = assembly_last.then(|| designs.len().saturating_sub(1));
    sink.set_phase("probe");
    let mut profile = load_profile(a)?;
    let mut client = spawn(a, &[steps_dir])?;
    let (catalog, report) = Runner::probe(&mut client, &mut profile)?;
    eprintln!("SolidCraft {} ({} commands)", catalog.version.as_deref().unwrap_or("?"), catalog.commands.len());
    for r in &report {
        eprintln!("  profile: {r}");
    }
    if a.batch > 1 && profile.runner.batch.is_none() {
        eprintln!("  the profile has no `runner.batch` section: steps go one at a time");
    }
    sink.set_phase(if assembly_last && designs.len() > 1 { "parts" } else { "build" });
    let (runs, events) = {
        let mut runner = Runner::new(&mut client, &profile);
        runner.verbose = a.verbose;
        runner.timeout = a.timeout;
        runner.batch = a.batch;
        runner.steps_dir = Some(abs(steps_dir));
        runner.preset = preset;
        runner.observer = Some(&mut sink);
        let runs = runner.run_all(designs);
        (runs, runner.events.clone())
    };
    sink.finish(runs.iter().all(|r| r.saved));
    Ok((runs, report, events))
}

/// Absolute path text (without the `\\?\` prefix Windows' canonicalize adds).
fn abs(p: &Path) -> String {
    let p = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.to_path_buf()) };
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Write each part's STEP into `<out>/parts` (copied from the bundle, or cut out of the source
/// STEP) and return the paths by definition.
fn write_part_steps(a: &Analysis, out: &Path, skipped: &mut Vec<String>) -> Result<BTreeMap<usize, PathBuf>, String> {
    let dir = out.join("parts");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let used: std::collections::BTreeSet<usize> = a.instances.iter().filter(|x| !a.model.defs[x.def].assembly).map(|x| x.def).collect();
    let mut out_paths = BTreeMap::new();
    let source = match &a.model.step_path {
        Some(p) => Some(lcw_step::StepFile::read(p).map_err(|e| e.to_string())?),
        None => None,
    };
    for d in used {
        let def = &a.model.defs[d];
        if def.solids.is_empty() {
            // Surfaces only (a reference sketch, a construction surface): no part to build.
            skipped.push(def.name.clone());
            continue;
        }
        let target = dir.join(format!("{}.stp", def.key));
        if let Some(src) = &def.step {
            std::fs::copy(src, &target).map_err(|e| format!("{}: {e}", src.display()))?;
        } else if let (Some(file), Some(pd)) = (&source, def.step_pd) {
            let text = file.extract_product(pd).map_err(|e| e.to_string())?;
            std::fs::write(&target, text).map_err(|e| format!("{}: {e}", target.display()))?;
        } else {
            continue;
        }
        out_paths.insert(d, target);
    }
    Ok(out_paths)
}

fn convert(a: &Args, input: &Path) -> Result<bool, String> {
    let t0 = std::time::Instant::now();
    let (model, turn) = load_upright(a, input)?;
    let source_app = model.source_app.clone();
    let mut analysis = analyze(model, &analysis_options(a));
    if turn.is_some() {
        analysis.notes.push("stood upright: the source has Y up, so the root is turned a quarter turn about X (Y up becomes SolidCraft's Z up, and the source's front view is SolidCraft's front view); `--keep-axes` keeps the source's axes".into());
    }
    let out = a.out.clone().unwrap_or_else(|| {
        let base = if input.is_dir() { input.to_path_buf() } else { input.parent().map(Path::to_path_buf).unwrap_or_default() };
        base.join("SolidCraft")
    });
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    std::fs::write(out.join(ORIENTATION_FILE), orientation_json(turn.as_ref(), &source_app).to_pretty()).map_err(|e| e.to_string())?;
    let mut skipped = Vec::new();
    let steps_by_def = write_part_steps(&analysis, &out, &mut skipped)?;
    for name in &skipped {
        analysis.notes.push(format!("{name} has no solid bodies (surfaces only) and is left out"));
    }
    let out_abs = abs(&out);
    let target = |p: &Path| -> String {
        let s = abs(p);
        if a.relative_paths {
            if let Some(rest) = s.strip_prefix(out_abs.as_str()) {
                return relative_path(rest);
            }
        }
        match &a.path_map {
            Some((from, to)) if s.starts_with(from.as_str()) => {
                let rest = &s[from.len()..];
                let rest = if to.contains('\\') { rest.replace('/', "\\") } else { rest.to_string() };
                format!("{to}{rest}")
            }
            _ => s,
        }
    };
    let eo = EmitOptions { out_dir: target(&out), sheet_metal: a.sheet_metal, find_limits: a.limits, parametric_patterns: a.patterns, ..EmitOptions::default() };
    let mut parts = Vec::new();
    let mut part_defs = Vec::new();
    let mut part_files = BTreeMap::new();
    for (def, step) in &steps_by_def {
        let file = target(&out.join("parts").join(format!("{}.sc", analysis.model.defs[*def].key)));
        parts.push(part_steps(&analysis, *def, &target(step), &file, &eo));
        part_defs.push(*def);
        part_files.insert(*def, file);
    }
    let asm_file = target(&out.join(format!("{}.sc", analysis.model.defs[analysis.model.root].key)));
    let (assembly, limits) = assembly_plan(&analysis, &part_files, &asm_file, &eo);
    analysis.notes.extend(assembly.notes.iter().cloned());
    let run_limits = a.limits && limits.is_some();
    let plan = BuildPlan { parts, assembly, limits, run_limits, part_defs };
    std::fs::write(out.join("build-steps.json"), plan.to_json().to_pretty()).map_err(|e| e.to_string())?;
    eprintln!(
        "analysed {} in {:.1}s: {} parts, {} sheet metal, {} joints, {} constraints, {} patterns",
        input.display(),
        t0.elapsed().as_secs_f64(),
        plan.parts.len(),
        analysis.sheets.len(),
        analysis.joints.joints.len(),
        analysis.joints.constraints.len(),
        analysis.patterns.len()
    );
    let mut runs: Vec<DesignResult> = Vec::new();
    let mut probe_report = Vec::new();
    let mut events = Vec::new();
    if !a.dry_run {
        let mut designs: Vec<&DesignSteps> = plan.parts.iter().chain(std::iter::once(&plan.assembly)).collect();
        if run_limits {
            designs.extend(plan.limits.iter());
        }
        // A new build: what an earlier limits stage found in this folder is for another design.
        let _ = std::fs::remove_file(out.join(LIMITS_RECORD));
        (runs, probe_report, events) = build(a, &out, &out, &designs, true)?;
        if let Some(asm) = runs.iter().find(|r| r.name == plan.assembly.name) {
            write_ids(&out, asm)?;
        }
    }
    let md = report::markdown(&analysis, &runs, &probe_report, &events);
    std::fs::write(out.join("conversion-report.md"), &md).map_err(|e| e.to_string())?;
    std::fs::write(out.join("conversion-report.json"), report::json(&analysis, &runs, &events).to_pretty()).map_err(|e| e.to_string())?;
    eprintln!("report: {}", out.join("conversion-report.md").display());
    if a.dry_run {
        eprintln!("dry run: build steps in {}", out.join("build-steps.json").display());
        return Ok(true);
    }
    let ok = runs.iter().all(|r| r.saved);
    eprintln!("{} → {}", if ok { "done" } else { "finished with problems" }, asm_file);
    Ok(ok)
}

/// The ids the assembly's build bound (`assembly-ids.json`), for `inv2sc limits` later.
fn write_ids(out: &Path, asm: &DesignResult) -> Result<(), String> {
    let mut ids = Json::obj();
    for (k, v) in &asm.vars {
        if k != lcw_solidcraft::steps::STEPS_DIR {
            ids.set(k, v.clone());
        }
    }
    let j = lcw_json::obj! {"schema" => "lcw-assembly-ids/1", "assembly" => asm.file.as_str(), "saved" => asm.saved, "ids" => ids};
    std::fs::write(out.join(IDS_FILE), j.to_pretty()).map_err(|e| e.to_string())
}

const IDS_FILE: &str = "assembly-ids.json";

/// Find the travel limits of a built assembly's joints: the limits stage of its build steps,
/// with the ids its build bound.
fn limits(a: &Args, built: &Path) -> Result<bool, String> {
    let steps_file = built.join("build-steps.json");
    let text = std::fs::read_to_string(&steps_file).map_err(|e| format!("{}: {e}", steps_file.display()))?;
    let plan = Json::parse(&text).map_err(|e| format!("{}: {e}", steps_file.display()))?;
    let Some(stage) = limits_from_json(&plan)? else {
        eprintln!("the assembly has no joint whose travel can be searched (build-steps.json has no limits stage)");
        return Ok(true);
    };
    let ids_text = std::fs::read_to_string(built.join(IDS_FILE)).map_err(|e| format!("{}: {e} (the ids a build bound; written by convert from this version on)", built.join(IDS_FILE).display()))?;
    let ids = Json::parse(&ids_text).map_err(|e| e.to_string())?;
    if ids.bool_of("saved") == Some(false) {
        return Err("the assembly's build did not save it, so there is nothing to search".into());
    }
    let preset: BTreeMap<String, Json> = ids.get("ids").and_then(Json::as_object).unwrap_or(&[]).iter().cloned().collect();
    if a.redo {
        let _ = std::fs::remove_file(built.join(LIMITS_RECORD));
    }
    let out = a.out.clone().unwrap_or_else(|| built.to_path_buf());
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let (runs, probe_report, events) = build_with(a, &out, built, &[&stage], false, preset)?;
    let mut md = format!("# Joint limits of {}\n\n", stage.file);
    for p in &probe_report {
        md.push_str(&format!("- profile: {p}\n"));
    }
    md.push_str(&report::limits_section(&runs));
    md.push_str(&report::build_section(&runs, &events));
    std::fs::write(out.join("limits-report.md"), md).map_err(|e| e.to_string())?;
    eprintln!("report: {}", out.join("limits-report.md").display());
    Ok(runs.iter().all(|r| r.saved))
}

fn analyze_only(a: &Args, input: &Path) -> Result<bool, String> {
    let analysis = analyze(load_upright(a, input)?.0, &analysis_options(a));
    println!("{}", report::markdown(&analysis, &[], &[], &[]));
    Ok(true)
}

/// Build from a `build-steps.json`: paths written relative to it (`--relative-paths`) are
/// resolved against its folder. The run report (`run-report.md`/`.json`) goes next to it.
fn run_steps(a: &Args, steps: &Path) -> Result<bool, String> {
    let text = std::fs::read_to_string(steps).map_err(|e| format!("{}: {e}", steps.display()))?;
    let plan = Json::parse(&text).map_err(|e| format!("{}: {e}", steps.display()))?;
    let designs = designs_from_json(&plan)?;
    let dir = steps.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let out = a.out.clone().unwrap_or_else(|| dir.clone());
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let refs: Vec<&DesignSteps> = designs.iter().collect();
    let (runs, probe_report, events) = build(a, &out, &dir, &refs, plan.get("assembly").is_some_and(|x| !x.is_null()))?;
    let mut md = format!("# Build of {}\n\n", steps.display());
    for p in &probe_report {
        md.push_str(&format!("- profile: {p}\n"));
    }
    md.push_str(&report::build_section(&runs, &events));
    std::fs::write(out.join("run-report.md"), md).map_err(|e| e.to_string())?;
    std::fs::write(out.join("run-report.json"), lcw_json::obj! {"schema" => "lcw-run-report/1", "steps" => abs(steps), "events" => events, "builds" => report::builds_json(&runs)}.to_pretty()).map_err(|e| e.to_string())?;
    eprintln!("report: {}", out.join("run-report.md").display());
    Ok(runs.iter().all(|r| r.saved))
}

/// The turn verify puts the bundle's placements through: `--root-turn`, else the built folder's
/// orientation.json (a rigid rotation, checked here), else none.
fn root_turn_for_verify(a: &Args, built: &Path) -> Result<(lcw_geom::M4, String), String> {
    use lcw_verify::expect::Y_UP;
    match a.root_turn.as_deref() {
        Some("y-up") => return Ok((Y_UP, "Y up to Z up, a quarter turn about X (--root-turn y-up)".into())),
        Some(_) => return Ok((lcw_geom::M4::IDENTITY, "none (--root-turn none)".into())),
        None => {}
    }
    let f = built.join(ORIENTATION_FILE);
    let Ok(text) = std::fs::read_to_string(&f) else { return Ok((lcw_geom::M4::IDENTITY, format!("none (no {ORIENTATION_FILE} in the built folder)"))) };
    let j = Json::parse(&text).map_err(|e| format!("{}: {e}", f.display()))?;
    let v: Vec<f64> = j.arr_of("root_turn").iter().filter_map(Json::as_f64).collect();
    let m = lcw_geom::M4::from_slice(&v).ok_or_else(|| format!("{}: `root_turn` is not a 4x4 matrix", f.display()))?;
    // A rotation about the origin, nothing else.
    let r = m.0;
    let ortho = (0..3).all(|i| (0..3).all(|k| ((0..3).map(|n| r[i][n] * r[k][n]).sum::<f64>() - if i == k { 1.0 } else { 0.0 }).abs() < 1e-9));
    let det = r[0][0] * (r[1][1] * r[2][2] - r[1][2] * r[2][1]) - r[0][1] * (r[1][0] * r[2][2] - r[1][2] * r[2][0]) + r[0][2] * (r[1][0] * r[2][1] - r[1][1] * r[2][0]);
    if !ortho || (det - 1.0).abs() > 1e-9 || (0..3).any(|i| r[i][3].abs() > 1e-9 || r[3][i].abs() > 1e-9) {
        return Err(format!("{}: `root_turn` is not a rotation about the origin", f.display()));
    }
    let what = if (0..4).all(|i| (0..4).all(|k| (m.0[i][k] - Y_UP.0[i][k]).abs() < 1e-12)) {
        "Y up to Z up, a quarter turn about X"
    } else if m == lcw_geom::M4::IDENTITY {
        "none"
    } else {
        "a rotation"
    };
    Ok((m, format!("{what} (from {ORIENTATION_FILE}: rows {:?})", m.0.iter().take(3).map(|r| [r[0], r[1], r[2]]).collect::<Vec<_>>())))
}

/// Check a finished build against the bundle it came from.
fn verify(a: &Args, input: &Path) -> Result<bool, String> {
    let bundle = lcw_bundle::Bundle::read(input).map_err(|e| e.to_string())?;
    let root_name = bundle.root_doc().map(|d| d.name.clone()).ok_or("the bundle has no root document")?;
    let base = if input.is_dir() { input.to_path_buf() } else { input.parent().map(Path::to_path_buf).unwrap_or_default() };
    let built = a.built.clone().unwrap_or_else(|| base.join("SolidCraft"));
    let assembly = match &a.assembly {
        Some(p) => p.clone(),
        None => {
            // The converter names the assembly after the top document; its file name may have
            // been made safe, so fall back to the only .sc file beside the parts folder.
            let direct = built.join(format!("{root_name}.sc"));
            if direct.is_file() {
                direct
            } else {
                let found: Vec<PathBuf> = std::fs::read_dir(&built)
                    .map_err(|e| format!("{}: {e}", built.display()))?
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "sc"))
                    .collect();
                match found.as_slice() {
                    [one] => one.clone(),
                    _ => return Err(format!("no assembly design `{root_name}.sc` in {}; pass --assembly FILE", built.display())),
                }
            }
        }
    };
    let out = a.out.clone().unwrap_or_else(|| built.clone());
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut tol = lcw_verify::Tol::default();
    if let Some(x) = a.tol_mm {
        tol.pos_mm = x;
        tol.bbox_mm = x;
    }
    if let Some(x) = a.tol_deg {
        tol.angle_deg = x;
    }
    if let Some(x) = a.clash_budget {
        tol.interference_budget_s = x;
    }
    let mut profile = load_profile(a)?;
    let bundle_dir = bundle.folder.clone().unwrap_or_else(|| base.clone());
    let mut client = spawn(a, &[&built, &bundle_dir])?;
    let (_, probe_report) = Runner::probe(&mut client, &mut profile)?;
    for r in &probe_report {
        eprintln!("  profile: {r}");
    }
    if a.no_clash {
        profile.disabled.insert("interference".into(), "skipped (--no-clash)".into());
    }
    let quiet = a.quiet;
    let log = move |m: &str| {
        if !quiet {
            eprintln!("verify: {m}");
        }
    };
    let (root_turn, root_turn_note) = root_turn_for_verify(a, &built)?;
    eprintln!("verify: root turn {root_turn_note}");
    let mut session = lcw_verify::session::Session::new(&mut client, &profile, a.timeout.unwrap_or(DEFAULT_TIMEOUT));
    let report = lcw_verify::run(&mut session, &lcw_verify::Inputs { bundle: &bundle, built: &built, assembly: &assembly, tol: tol.clone(), root_turn, root_turn_note, log: &log })?;
    std::fs::write(out.join("verification-report.md"), lcw_verify::report::markdown(&report, &tol)).map_err(|e| e.to_string())?;
    std::fs::write(out.join("verification.json"), lcw_verify::report::json(&report, &tol).to_pretty()).map_err(|e| e.to_string())?;
    println!("{}", report.summary_line());
    println!("report: {}", out.join("verification-report.md").display());
    Ok(!report.failed())
}

fn probe(a: &Args) -> Result<bool, String> {
    let mut profile = load_profile(a)?;
    let mut client = spawn(a, &[])?;
    let (catalog, report) = Runner::probe(&mut client, &mut profile)?;
    println!("SolidCraft {} — {} commands; profile tested with {:?}", catalog.version.as_deref().unwrap_or("?"), catalog.commands.len(), profile.tested_with);
    if report.is_empty() {
        println!("every operation in the profile matches this SolidCraft");
    }
    for r in &report {
        println!("- {r}");
    }
    let _ = client.call("version", Json::obj());
    Ok(report.iter().all(|r| !r.contains("skipped")))
}

/// Convert every bundle that appears in `folder` (a subfolder with bundle.json and a `ready`
/// marker the exporter writes last), once.
fn watch(a: &Args, folder: &Path) -> Result<bool, String> {
    eprintln!("watching {} for Inventor exports (Ctrl+C to stop)", folder.display());
    let mut done: BTreeMap<PathBuf, SystemTime> = BTreeMap::new();
    loop {
        if let Ok(entries) = std::fs::read_dir(folder) {
            for e in entries.flatten() {
                let dir = e.path();
                let ready = dir.join("ready");
                if !dir.join(lcw_bundle::FILE_NAME).is_file() || !ready.is_file() {
                    continue;
                }
                let stamp = std::fs::metadata(&ready).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                if done.get(&dir) == Some(&stamp) {
                    continue;
                }
                done.insert(dir.clone(), stamp);
                let mut args = Args { input: Some(dir.clone()), out: None, ..clone_args(a) };
                args.out = a.out.as_ref().map(|o| o.join(dir.file_name().unwrap_or_default()));
                match convert(&args, &dir) {
                    Ok(_) => eprintln!("converted {}", dir.display()),
                    Err(e) => eprintln!("{}: {e}", dir.display()),
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn clone_args(a: &Args) -> Args {
    Args {
        cmd: a.cmd.clone(),
        input: a.input.clone(),
        out: a.out.clone(),
        cli: a.cli.clone(),
        connect: a.connect.clone(),
        profile: a.profile.clone(),
        dry_run: a.dry_run,
        merge: a.merge,
        sheet_metal: a.sheet_metal,
        infer: a.infer,
        limits: a.limits,
        inventor_constraints: a.inventor_constraints,
        patterns: a.patterns,
        max_thickness: a.max_thickness,
        verbose: a.verbose,
        path_map: a.path_map.clone(),
        relative_paths: a.relative_paths,
        timeout: a.timeout,
        batch: a.batch,
        quiet: a.quiet,
        built: a.built.clone(),
        assembly: a.assembly.clone(),
        tol_mm: a.tol_mm,
        tol_deg: a.tol_deg,
        clash_budget: a.clash_budget,
        no_clash: a.no_clash,
        redo: a.redo,
        up: a.up.clone(),
        root_turn: a.root_turn.clone(),
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let need_input = || args.input.clone().ok_or_else(|| format!("`{}` needs an input\n\n{USAGE}", args.cmd));
    let result = match args.cmd.as_str() {
        "convert" => need_input().and_then(|i| convert(&args, &i)),
        "analyze" | "analyse" => need_input().and_then(|i| analyze_only(&args, &i)),
        "watch" => need_input().and_then(|i| watch(&args, &i)),
        "run" => need_input().and_then(|i| run_steps(&args, &i)),
        "limits" => need_input().and_then(|i| limits(&args, &i)),
        "probe" => probe(&args),
        "verify" => need_input().and_then(|i| verify(&args, &i)),
        "version" | "--version" => {
            println!("inv2sc {}", env!("CARGO_PKG_VERSION"));
            Ok(true)
        }
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
