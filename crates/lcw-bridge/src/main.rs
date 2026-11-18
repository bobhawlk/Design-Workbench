//! inv2sc: Inventor (or STEP) assemblies into clean, editable SolidCraft assemblies.

mod report;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use lcw_analyze::model::Model;
use lcw_analyze::{analyze, Analysis, Merge, Options};
use lcw_json::Json;
use lcw_solidcraft::emit::{assembly_steps, part_steps, BuildPlan, Options as EmitOptions};
use lcw_solidcraft::mcp::{McpClient, Transport};
use lcw_solidcraft::profile::Profile;
use lcw_solidcraft::run::{DesignResult, Runner};

const USAGE: &str = "inv2sc — Inventor assemblies into editable SolidCraft assemblies

USAGE:
  inv2sc convert <bundle-folder | bundle.json | assembly.stp> [options]
  inv2sc analyze <bundle-folder | bundle.json | assembly.stp> [options]   (no SolidCraft needed)
  inv2sc watch <folder> [options]       convert each bundle that appears in <folder>
  inv2sc probe [--solidcraft-cli PATH] [--connect HOST:PORT] [--profile FILE]
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
  --no-limits             don't search each joint's travel for its limits
  --no-patterns           don't make pattern spacing a parameter
  --max-thickness MM      thickest plate treated as sheet metal (default 6)
  --path-map FROM=TO      write paths for another machine: build steps made here name
                          FROM... as TO... (e.g. /tmp/out=Z:\\exports\\console), for running the
                          steps with Run-BuildSteps.ps1 where SolidCraft is
  --verbose               print each step
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
    patterns: bool,
    max_thickness: f64,
    verbose: bool,
    path_map: Option<(String, String)>,
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
        limits: true,
        patterns: true,
        max_thickness: 6.0,
        verbose: false,
        path_map: None,
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
            "--no-patterns" => a.patterns = false,
            "--max-thickness" => a.max_thickness = value("--max-thickness")?.parse().map_err(|_| "--max-thickness takes a number of mm")?,
            "--verbose" | "-v" => a.verbose = true,
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
    let is_bundle = input.is_dir() || input.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_bundle {
        let b = lcw_bundle::Bundle::read(input).map_err(|e| e.to_string())?;
        Model::from_bundle(&b)
    } else {
        Model::from_step(input)
    }
}

fn analysis_options(a: &Args) -> Options {
    let mut o = Options { merge: a.merge, ..Options::default() };
    o.sheet.max_thickness = a.max_thickness;
    o.joints.infer = a.infer;
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
    McpClient::spawn(&cli, &args)
}

/// Absolute path text (without the `\\?\` prefix Windows' canonicalize adds).
fn abs(p: &Path) -> String {
    let p = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.to_path_buf()) };
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Write each part's STEP into `<out>/parts` (copied from the bundle, or cut out of the source
/// STEP) and return the paths by definition.
fn write_part_steps(a: &Analysis, out: &Path) -> Result<BTreeMap<usize, PathBuf>, String> {
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
    let model = load_model(input)?;
    let analysis = analyze(model, &analysis_options(a));
    let out = a.out.clone().unwrap_or_else(|| {
        let base = if input.is_dir() { input.to_path_buf() } else { input.parent().map(Path::to_path_buf).unwrap_or_default() };
        base.join("SolidCraft")
    });
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let steps_by_def = write_part_steps(&analysis, &out)?;
    let target = |p: &Path| -> String {
        let s = abs(p);
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
    let assembly = assembly_steps(&analysis, &part_files, &asm_file, &eo);
    let plan = BuildPlan { parts, assembly, part_defs };
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
    if !a.dry_run {
        let mut profile = load_profile(a)?;
        let mut client = spawn(a, &[&out])?;
        let (catalog, report) = Runner::probe(&mut client, &mut profile)?;
        eprintln!("SolidCraft {} ({} commands)", catalog.version.as_deref().unwrap_or("?"), catalog.commands.len());
        for r in &report {
            eprintln!("  profile: {r}");
        }
        probe_report = report;
        let mut runner = Runner::new(&mut client, &profile);
        runner.verbose = a.verbose;
        for (i, part) in plan.parts.iter().enumerate() {
            eprintln!("[{}/{}] {}", i + 1, plan.parts.len(), part.name);
            runs.push(runner.run(part));
        }
        eprintln!("assembly {}", plan.assembly.name);
        runs.push(runner.run(&plan.assembly));
    }
    let md = report::markdown(&analysis, &runs, &probe_report);
    std::fs::write(out.join("conversion-report.md"), &md).map_err(|e| e.to_string())?;
    std::fs::write(out.join("conversion-report.json"), report::json(&analysis, &runs).to_pretty()).map_err(|e| e.to_string())?;
    eprintln!("report: {}", out.join("conversion-report.md").display());
    if a.dry_run {
        eprintln!("dry run: build steps in {}", out.join("build-steps.json").display());
        return Ok(true);
    }
    let ok = runs.iter().all(|r| r.saved);
    eprintln!("{} → {}", if ok { "done" } else { "finished with problems" }, asm_file);
    Ok(ok)
}

fn analyze_only(a: &Args, input: &Path) -> Result<bool, String> {
    let analysis = analyze(load_model(input)?, &analysis_options(a));
    println!("{}", report::markdown(&analysis, &[], &[]));
    Ok(true)
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
        patterns: a.patterns,
        max_thickness: a.max_thickness,
        verbose: a.verbose,
        path_map: a.path_map.clone(),
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
        "probe" => probe(&args),
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
