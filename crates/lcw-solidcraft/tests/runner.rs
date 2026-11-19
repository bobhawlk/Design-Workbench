//! The step runner against a scripted fake SolidCraft (`Recorder`): progress and the build log
//! as it goes, timeouts and what happens to the session after one, joint limits, steps left
//! out after a failure, paths relative to the steps file, and batching.
//!
//! Every fixture here is invented.

use std::path::PathBuf;
use std::time::Duration;

use lcw_json::{obj, Json};
use lcw_solidcraft::mcp::{Action, Recorder};
use lcw_solidcraft::profile::Profile;
use lcw_solidcraft::progress::ProgressSink;
use lcw_solidcraft::run::{DesignResult, Observer, Runner, Status, StepResult};
use lcw_solidcraft::steps::{designs_from_json, relative_path, var, DesignSteps, Step};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("inv2sc-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A small invented part: import, sheet metal, properties, save.
fn part(name: &str) -> DesignSteps {
    DesignSteps {
        name: name.into(),
        file: format!("{name}.sc"),
        steps: vec![
            Step::tool("new_design", Json::obj(), format!("new design for {name}")).group("new design"),
            Step::op("insert_step", obj! {"path" => format!("/in/{name}.stp"), "name" => name}, format!("import {name}")).bind("body", &["/created_bodies/0"]).bind("comp", &["/component"]).group("import"),
            Step::op("sheet_rule", obj! {"name" => "2 mm"}, "sheet metal rule").soft().group("sheet metal"),
            Step::op("sheet_convert", obj! {"body" => var("body"), "face" => [0.0, 0.0, 0.0], "bends" => vec![Json::from([1.0, 0.0, 0.0]), Json::from([5.0, 5.0, 5.0])]}, format!("make {name} sheet metal")).soft().needs("body").group("sheet metal"),
            Step::op("component_metadata", obj! {"component" => var("comp")}, "part properties").soft().group("properties"),
            Step::tool("save", obj! {"path" => format!("/out/{name}.sc")}, format!("save {name}")).group("save"),
        ],
        notes: Vec::new(),
    }
}

/// An invented assembly: two parts placed, a joint with its limits, a constraint, save.
fn assembly() -> DesignSteps {
    let mut steps = vec![Step::tool("new_design", Json::obj(), "new design for Rig").group("new design")];
    for (i, p) in ["Arm", "Base"].iter().enumerate() {
        let v = format!("occ:{i}");
        steps.push(Step::op("insert_design", obj! {"path" => format!("/out/{p}.sc"), "name" => *p}, format!("place {p}")).bind(&v, &["/occurrence"]).group("components"));
        steps.push(Step::op("place", obj! {"occurrence" => var(&v), "to_matrix" => vec![1.0; 16]}, format!("place {p}:1")).group("placement"));
    }
    steps.push(Step::op("ground", obj! {"occurrence" => var("occ:1"), "grounded" => true}, "ground Base").soft().group("grounding"));
    steps.push(Step::op("joint", obj! {"type" => "revolute", "a" => obj!{"occurrence" => var("occ:1")}, "b" => obj!{"occurrence" => var("occ:0")}, "name" => "Hinge"}, "revolute Hinge").soft().group("joints"));
    steps.push(Step::op("find_limits", obj! {"joint" => "Hinge"}, "find how far Hinge can move").soft().bind("limits:Hinge", &["/values/0"]).group("limits"));
    steps.push(Step::tool("auto_limits", obj! {"joint" => "Hinge", "found" => var("limits:Hinge")}, "limit Hinge").soft().needs("limits:Hinge").group("limits"));
    steps.push(
        Step::op("constraint", obj! {"type" => "mate", "a" => obj!{"occurrence" => var("occ:0")}}, "mate M1")
            .soft()
            .or_else("joint", obj! {"type" => "planar", "a" => obj!{"occurrence" => var("occ:0")}})
            .group("constraints"),
    );
    steps.push(Step::tool("save", obj! {"path" => "/out/Rig.sc"}, "save Rig").group("save"));
    DesignSteps { name: "Rig".into(), file: "/out/Rig.sc".into(), steps, notes: Vec::new() }
}

fn statuses(r: &DesignResult) -> Vec<&'static str> {
    r.steps.iter().map(|s| s.status.name()).collect()
}

fn run_with(rec: &mut Recorder, designs: &[&DesignSteps], f: impl FnOnce(&mut Runner)) -> (Vec<DesignResult>, Vec<String>) {
    let profile = Profile::builtin();
    let mut runner = Runner::new(rec, &profile);
    f(&mut runner);
    let r = runner.run_all(designs);
    (r, runner.events.clone())
}

// ---- 1, 2: progress, timing and the log as it goes ----

/// Reads the log and the progress file from disk after every step, while the run goes on.
struct Peek {
    inner: ProgressSink,
    log: PathBuf,
    progress: PathBuf,
    seen: Vec<(usize, usize, Json)>,
}

impl Observer for Peek {
    fn design_started(&mut self, d: &DesignSteps) {
        self.inner.design_started(d)
    }
    fn step_finished(&mut self, d: &DesignSteps, i: usize, r: &StepResult) {
        self.inner.step_finished(d, i, r);
        let log = std::fs::read_to_string(&self.log).unwrap();
        let steps = log.lines().filter(|l| l.contains(r#""event":"step""#)).count();
        let progress = Json::parse(&std::fs::read_to_string(&self.progress).unwrap()).unwrap();
        self.seen.push((i + 1, steps, progress));
    }
    fn design_finished(&mut self, r: &DesignResult) {
        self.inner.design_finished(r)
    }
}

#[test]
fn each_step_is_logged_flushed_and_timed_as_it_finishes() {
    let dir = tmp("progress");
    let (log, progress) = (dir.join("build-log.jsonl"), dir.join("progress.json"));
    let mut sink = ProgressSink::new(Some(progress.clone()), Some(log.clone())).unwrap();
    sink.print = false;
    sink.interval = Duration::ZERO;
    sink.design_total = 2;
    sink.assembly_index = Some(1);
    let mut peek = Peek { inner: sink, log: log.clone(), progress: progress.clone(), seen: Vec::new() };
    let mut rec = Recorder::new().on("joint.find_limits", Action::Delay(Duration::from_millis(120)), 0);
    rec.fail.push("sheet.manage_rules".into());
    let (p, a) = (part("Arm"), assembly());
    let profile = Profile::builtin();
    let runs = {
        let mut runner = Runner::new(&mut rec, &profile);
        runner.observer = Some(&mut peek);
        runner.run_all(&[&p, &a])
    };
    // On disk after every step: the log has that step, the progress file says where the run is.
    let mut before = 0;
    for (n, (k, logged, prog)) in peek.seen.iter().enumerate() {
        assert_eq!(prog.f64_of("step"), Some(*k as f64), "{prog}");
        assert_eq!(*logged, n + 1, "after step {k}: {logged} step records on disk");
        assert!(*logged > before);
        before = *logged;
    }
    assert_eq!(peek.seen.len(), p.steps.len() + a.steps.len());
    let first = &peek.seen[0].2;
    assert_eq!((first.str_of("phase"), first.str_of("design"), first.f64_of("design_total")), (Some("parts"), Some("Arm"), Some(2.0)));
    let failed_rule = &peek.seen[2].2;
    assert_eq!(failed_rule.f64_of("failures"), Some(1.0), "failures so far: {failed_rule}");
    let in_asm = &peek.seen[p.steps.len()].2;
    assert_eq!((in_asm.str_of("phase"), in_asm.f64_of("design_index")), (Some("assembly"), Some(2.0)));
    peek.inner.finish(true);
    let done = Json::parse(&std::fs::read_to_string(&progress).unwrap()).unwrap();
    assert_eq!(done.str_of("state"), Some("finished"));
    assert!(done.f64_of("elapsed_s").is_some());
    // The slow command is timed, in the result and in the log.
    let fl = runs[1].steps.iter().find(|s| s.op == "find_limits").unwrap();
    assert!(fl.ms >= 120.0 && fl.command == "joint.find_limits", "{fl:?}");
    let text = std::fs::read_to_string(&log).unwrap();
    let rec_line = text.lines().map(|l| Json::parse(l).unwrap()).find(|j| j.str_of("op") == Some("find_limits")).unwrap();
    assert!(rec_line.f64_of("ms").unwrap() >= 120.0, "{rec_line}");
    assert_eq!(text.lines().filter(|l| l.contains(r#""event":"design_done""#)).count(), 2);
    assert!(text.lines().last().unwrap().contains("run_done"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn progress_lines_are_throttled_but_failures_always_show() {
    let dir = tmp("throttle");
    let mut sink = ProgressSink::new(None, None).unwrap();
    sink.interval = Duration::from_secs(3600);
    sink.design_total = 1;
    let mut rec = Recorder::new();
    rec.fail.push("joint.create".into());
    let a = assembly();
    let profile = Profile::builtin();
    {
        let mut runner = Runner::new(&mut rec, &profile);
        runner.observer = Some(&mut sink);
        runner.run(&a);
    }
    // The design line, the first step, the failed joint, the last step, the summary: not all
    // twelve steps.
    assert!(sink.printed >= 4 && sink.printed < a.steps.len(), "{} lines", sink.printed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crash_keeps_the_log_up_to_the_step_it_stopped_at() {
    let dir = tmp("crash");
    let log = dir.join("build-log.jsonl");
    let mut sink = ProgressSink::new(None, Some(log.clone())).unwrap();
    sink.print = false;
    let mut rec = Recorder::new().on("occurrence.move", Action::Die, 1);
    let a = assembly();
    let profile = Profile::builtin();
    let r = {
        let mut runner = Runner::new(&mut rec, &profile);
        runner.observer = Some(&mut sink);
        runner.run(&a)
    };
    // The run went on to the end of the design (its steps reported as not run), and everything
    // before the crash is on disk already, without the sink having been told the run is over.
    let text = std::fs::read_to_string(&log).unwrap();
    let steps: Vec<Json> = text.lines().map(|l| Json::parse(l).unwrap()).filter(|j| j.str_of("event") == Some("step")).collect();
    assert_eq!(steps.len(), a.steps.len());
    assert_eq!(steps[2].str_of("status"), Some("failed"));
    assert!(steps[2].str_of("detail").unwrap().contains("stopped"), "{}", steps[2]);
    assert!(r.session_lost.is_some() && !r.saved);
    drop(sink);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 3: timeouts and the session afterwards ----

#[test]
fn a_call_that_hangs_fails_its_step_ends_the_session_and_the_next_design_gets_a_new_one() {
    // (`sheet.manage_rules` has no timeout of its own, so the 40 ms floor is its limit.)
    let mut rec = Recorder::new().on("sheet.manage_rules", Action::Hang, 1);
    let (p1, p2, a) = (part("Arm"), part("Base"), assembly());
    let (runs, events) = run_with(&mut rec, &[&p1, &p2, &a], |r| r.timeout = Some(Duration::from_millis(40)));
    let r = &runs[0];
    let rule = &r.steps[2];
    assert!(matches!(&rule.status, Status::Failed(e) if e.contains("no answer") && e.contains("session was given up")), "{:?}", rule.status);
    assert_eq!(statuses(r)[3..], ["not run", "not run", "not run"], "the rest, save included, is not run in a session that was given up");
    assert!(matches!(&r.steps[5].status, Status::NotRun(w) if w.contains("given up")));
    assert!(!r.saved && r.session_lost.is_some());
    assert_eq!(rec.restarts, 1);
    assert!(events.iter().any(|e| e.contains("started again before Base")), "{events:?}");
    assert!(runs[1].saved && runs[2].saved, "the next designs build in the new session");
}

#[test]
fn the_slow_ops_get_their_own_longer_timeout() {
    // `find_limits` has 900 s in the profile; a 60 ms floor doesn't shorten it.
    let mut rec = Recorder::new().on("joint.find_limits", Action::Delay(Duration::from_millis(150)), 0);
    let a = assembly();
    let (runs, _) = run_with(&mut rec, &[&a], |r| r.timeout = Some(Duration::from_millis(60)));
    assert!(runs[0].saved);
    assert_eq!(runs[0].steps.iter().find(|s| s.op == "find_limits").unwrap().status, Status::Done);
    // A plain op with the same delay times out.
    let mut rec = Recorder::new().on("occurrence.ground", Action::Delay(Duration::from_millis(150)), 0);
    let (runs, _) = run_with(&mut rec, &[&a], |r| r.timeout = Some(Duration::from_millis(60)));
    assert!(matches!(&runs[0].steps.iter().find(|s| s.op == "ground").unwrap().status, Status::Failed(e) if e.contains("no answer")));
}

#[test]
fn when_solidcraft_cant_be_restarted_the_rest_is_not_run_and_says_why() {
    let mut rec = Recorder::new().on("file.insert_step", Action::Die, 1).on("restart", Action::Fail("x".into()), 1);
    let (p1, p2) = (part("Arm"), part("Base"));
    let (runs, _) = run_with(&mut rec, &[&p1, &p2], |_| {});
    assert!(runs[1].steps.iter().all(|s| matches!(&s.status, Status::NotRun(w) if w.contains("could not be started again"))));
}

#[test]
fn a_bridged_call_still_running_is_waited_for_then_cancelled() {
    // Finishes after two polls of `pending`: the step is done and its ids are bound.
    let mut rec = Recorder::new().on("component.insert_design", Action::StillRunning(2), 1);
    let a = assembly();
    let (runs, _) = run_with(&mut rec, &[&a], |_| {});
    assert!(runs[0].saved && runs[0].steps[1].status == Status::Done && runs[0].steps[2].status == Status::Done, "{:?}", statuses(&runs[0]));
    assert!(runs[0].steps[1].calls >= 3, "the call and the polls: {}", runs[0].steps[1].calls);
    // Never finishes, and SolidCraft cancels it: that step failed, the session goes on.
    let mut rec = Recorder::new().on("joint.create", Action::StillRunning(1_000_000), 1);
    rec.cancellable = true;
    let (runs, _) = run_with(&mut rec, &[&a], |r| r.timeout = Some(Duration::from_millis(80)));
    let j = runs[0].steps.iter().find(|s| s.label == "revolute Hinge").unwrap();
    assert!(matches!(&j.status, Status::Failed(e) if e.contains("cancelled")), "{:?}", j.status);
    assert!(runs[0].saved && runs[0].session_lost.is_none() && rec.restarts == 0);
    // Never finishes and can't be cancelled: the session is given up.
    let mut rec = Recorder::new().on("joint.create", Action::StillRunning(1_000_000), 1);
    let (runs, _) = run_with(&mut rec, &[&a], |r| r.timeout = Some(Duration::from_millis(80)));
    assert!(runs[0].session_lost.as_deref().is_some_and(|l| l.contains("could not be cancelled")), "{:?}", runs[0].session_lost);
}

// ---- 4: joint limits ----

fn limits_run(found: Json) -> StepResult {
    let mut rec = Recorder::new().on("joint.find_limits", Action::Answer(obj! {"values" => vec![found]}), 0);
    let a = assembly();
    let (runs, _) = run_with(&mut rec, &[&a], |_| {});
    let r = &runs[0];
    // Counted with the rest: every step of the design has a result.
    assert_eq!(r.steps.len(), a.steps.len());
    assert!(r.summary().starts_with(&format!("{} of {} steps done", r.steps.iter().filter(|s| s.status.achieved()).count(), a.steps.len())));
    r.steps.iter().find(|s| s.op == "auto_limits").unwrap().clone()
}

#[test]
fn limits_are_set_counted_and_reported_when_unknown() {
    let set = limits_run(obj! {"min" => -30.0, "max" => 45.0, "min_by" => obj!{"a" => "Arm", "b" => "Base"}, "max_by" => obj!{"a" => "Arm", "b" => "Stop"}});
    assert_eq!(set.status, Status::Done);
    assert_eq!(set.command, "joint.limits");
    // The last value a search that ran out of time checked is not a stop, nor is a closed loop's.
    let budget = limits_run(obj! {"min" => -30.0, "max" => 2.0, "min_by" => obj!{"a" => "Arm", "b" => "Base"}, "max_by" => "budget"});
    assert!(matches!(&budget.status, Status::Skipped(w) if w.contains("one end unknown") && w.contains("ran out of time")), "{:?}", budget.status);
    let looped = limits_run(obj! {"min" => -0.1, "max" => 0.1, "min_by" => "loop", "max_by" => "loop"});
    assert!(matches!(&looped.status, Status::Skipped(w) if w.contains("closed loop")), "{:?}", looped.status);
    let one = limits_run(obj! {"min" => -30.0, "max" => Json::Null, "min_by" => obj!{"a" => "Arm", "b" => "Base"}, "max_by" => "range"});
    assert!(matches!(&one.status, Status::Skipped(w) if w.contains("one end unknown") && w.contains("max unknown") && w.contains("where Arm meets Base")), "{:?}", one.status);
    let free = limits_run(obj! {"min" => -180.0, "max" => 180.0, "min_by" => "turn", "max_by" => "turn"});
    assert!(matches!(&free.status, Status::Skipped(w) if w.contains("nothing stops it")), "{:?}", free.status);
    let open = limits_run(obj! {"min" => Json::Null, "max" => Json::Null, "min_by" => "range", "max_by" => "range"});
    assert!(matches!(&open.status, Status::Skipped(w) if w.contains("nothing stops it")), "{:?}", open.status);
    let odd = limits_run(obj! {"index" => 0});
    assert!(matches!(&odd.status, Status::Skipped(w) if w.contains("no min or max")), "{:?}", odd.status);
}

// ---- 5: what a hard failure leaves out ----

#[test]
fn a_hard_failure_says_which_steps_were_left_out_and_why() {
    let mut rec = Recorder::new().on("occurrence.move", Action::Fail("occurrence 1101: not a rigid placement".into()), 1);
    let a = assembly();
    let (runs, _) = run_with(&mut rec, &[&a], |_| {});
    let r = &runs[0];
    assert!(matches!(&r.steps[2].status, Status::Failed(e) if e == "occurrence 1101: not a rigid placement"));
    let why = r.stopped.clone().unwrap();
    assert!(why.contains("step 3 (place Arm:1)"), "{why}");
    for s in &r.steps[3..a.steps.len() - 1] {
        assert_eq!(s.status, Status::NotRun(why.clone()), "{}", s.label);
    }
    assert!(r.saved, "what was built is still saved");
}

// ---- 7: paths relative to the steps file ----

#[test]
fn relative_paths_resolve_against_the_steps_folder_at_run_time() {
    let mut p = part("Arm");
    p.steps[1].args.set("path", relative_path("parts/Arm.stp"));
    p.steps[5].args.set("path", relative_path("parts/Arm.sc"));
    let file = obj! {"schema" => "lcw-build-steps/1", "parts" => vec![obj!{"name" => "Arm", "file" => relative_path("parts/Arm.sc"), "steps" => Json::Arr(p.steps.iter().map(Step::to_json).collect())}], "assembly" => Json::Null};
    let designs = designs_from_json(&Json::parse(&file.to_pretty()).unwrap()).unwrap();
    let mut rec = Recorder::new();
    let (runs, _) = run_with(&mut rec, &[&designs[0]], |r| r.steps_dir = Some("/moved/here".into()));
    assert!(runs[0].saved);
    let sep = std::path::MAIN_SEPARATOR;
    let import = rec.calls.iter().find(|(_, a)| a.str_of("command") == Some("file.insert_step")).unwrap();
    assert_eq!(import.1.pointer("params/path").and_then(Json::as_str), Some(format!("/moved/here{sep}parts{sep}Arm.stp").as_str()));
    let save = rec.calls.iter().find(|(t, _)| t == "save").unwrap();
    assert_eq!(save.1.str_of("path"), Some(format!("/moved/here{sep}parts{sep}Arm.sc").as_str()));
    // Without a folder to resolve against, nothing is sent with an unresolved path.
    let mut rec = Recorder::new();
    let (runs, _) = run_with(&mut rec, &[&designs[0]], |_| {});
    assert!(matches!(&runs[0].steps[1].status, Status::Skipped(w) if w.contains("steps_dir")));
}

// ---- 8: batching ----

#[test]
fn batching_gives_the_same_build_in_fewer_calls() {
    let (p1, p2, a) = (part("Arm"), part("Base"), assembly());
    let mut one = Recorder::new();
    let (r1, _) = run_with(&mut one, &[&p1, &p2, &a], |_| {});
    let mut many = Recorder::new();
    let (r2, _) = run_with(&mut many, &[&p1, &p2, &a], |r| r.batch = 50);
    for (x, y) in r1.iter().zip(&r2) {
        assert_eq!(statuses(x), statuses(y), "{}", x.name);
        assert!(y.saved);
    }
    assert_eq!(one.commands(), many.commands(), "the same commands, in the same order");
    assert!(many.batches > 0);
    let round_trips = |r: &Recorder| r.calls.iter().filter(|(t, _)| t != "batch step").count();
    assert!(round_trips(&many) < round_trips(&one), "{} vs {}", round_trips(&many), round_trips(&one));
    assert!(r2.iter().flat_map(|r| &r.steps).any(|s| s.batched && s.ms > 0.0), "batched steps carry SolidCraft's timing");
    // Ids bound in one batch are used by the next: the placement got the inserted occurrence.
    let place = many.calls.iter().find(|(_, a)| a.str_of("command") == Some("occurrence.move")).unwrap();
    assert!(place.1.pointer("params/occurrence").and_then(Json::as_f64).is_some_and(|v| v > 1000.0), "{}", place.1);
}

#[test]
fn a_step_failing_in_a_batch_gets_its_alternatives_and_retries() {
    // The constraint fails inside a batch and becomes the planar joint; the bend that closes a
    // loop is dropped and the conversion retried.
    let mut rec = Recorder::new()
        .on("assembly.constraint.create", Action::Fail("constraint: refused (test)".into()), 1)
        .on("sheet.convert", Action::Fail("sheet.convert: this bend closes a loop of bends at (5, 5, 5)".into()), 1);
    let (p, a) = (part("Arm"), assembly());
    let (runs, _) = run_with(&mut rec, &[&p, &a], |r| r.batch = 50);
    let conv = &runs[0].steps[3];
    assert!(matches!(&conv.status, Status::Fallback(w) if w.contains("[5,5,5]")), "{:?}", conv.status);
    let c = runs[1].steps.iter().find(|s| s.label == "mate M1").unwrap();
    assert_eq!(c.status, Status::Fallback("joint".into()));
    assert!(c.note.contains("constraint: refused"), "{}", c.note);
    assert!(runs.iter().all(|r| r.saved));
    // The retry sent the bends without the dropped one.
    let retry = rec.calls.iter().rfind(|(_, a)| a.str_of("command") == Some("sheet.convert")).unwrap();
    assert_eq!(retry.1.pointer("params/bends").map(Json::to_compact).as_deref(), Some("[[1,0,0]]"));
}

#[test]
fn a_hard_failure_in_a_batch_stops_the_design_as_it_would_alone() {
    let mut rec = Recorder::new().on("occurrence.move", Action::Fail("not rigid".into()), 1);
    let a = assembly();
    let (one, _) = run_with(&mut Recorder::new().on("occurrence.move", Action::Fail("not rigid".into()), 1), &[&a], |_| {});
    let (many, _) = run_with(&mut rec, &[&a], |r| r.batch = 50);
    assert_eq!(statuses(&one[0]), statuses(&many[0]));
    assert_eq!(one[0].stopped, many[0].stopped);
}

#[test]
fn a_batch_that_fails_without_saying_what_ran_ends_the_session_and_batching() {
    let mut rec = Recorder::new();
    rec.missing_tools.push("batch".into());
    let (p1, p2) = (part("Arm"), part("Base"));
    let (runs, events) = run_with(&mut rec, &[&p1, &p2], |r| r.batch = 50);
    assert!(runs[0].session_lost.is_some() && !runs[0].saved, "the design's state is unknown");
    assert!(events.iter().any(|e| e.contains("batching turned off")), "{events:?}");
    assert!(runs[1].saved && runs[1].steps.iter().all(|s| !s.batched), "the next design goes one step at a time");
    assert_eq!(rec.restarts, 1);
}

#[test]
fn a_batch_that_hangs_is_a_timeout_like_any_call() {
    let mut rec = Recorder::new().on("occurrence.move", Action::Hang, 1);
    let a = assembly();
    let (runs, _) = run_with(&mut rec, &[&a], |r| {
        r.batch = 50;
        r.timeout = Some(Duration::from_millis(30));
    });
    let failed: Vec<&StepResult> = runs[0].failures().collect();
    assert_eq!(failed.len(), 1);
    assert!(matches!(&failed[0].status, Status::Failed(e) if e.contains("batch of")), "{:?}", failed[0].status);
    assert!(runs[0].session_lost.is_some());
}

// ---- the mechanism check ----

#[test]
fn the_mechanism_check_drives_each_joint_and_reports_what_holds_it() {
    let listed = obj! {
        "dof" => vec![obj!{"occurrence" => 1, "dof" => 0}, obj!{"occurrence" => 2, "dof" => 1}, obj!{"occurrence" => 3, "dof" => 0}, obj!{"occurrence" => 4, "dof" => 6}],
        "fixed" => vec![1],
        "conflicts" => vec![obj!{"joint" => 9, "name" => "Lid stop", "why" => "off by 2 mm"}],
        "errors" => Json::Arr(Vec::new()),
        "floating" => vec![obj!{"occurrences" => vec!["Lamp:1"], "joints" => Json::Arr(Vec::new())}],
        "joints" => vec![
            obj!{"name" => "Hinge", "values" => vec![obj!{"name" => "angle", "value" => 0.0, "unit" => "deg", "limits" => vec![-30.0, 45.0]}]},
            obj!{"name" => "Lid", "values" => vec![obj!{"name" => "angle", "value" => 0.0, "unit" => "deg", "limits" => Json::Null}]},
            obj!{"name" => "Shut", "values" => vec![obj!{"name" => "angle", "value" => 0.0, "unit" => "deg", "limits" => vec![0.0, 0.0]}]},
        ],
    };
    let moved = obj! {"moved" => true, "values" => vec![obj!{"name" => "angle", "value" => 5.0, "unit" => "deg"}]};
    let stuck = obj! {"moved" => false, "reason" => "angle stopped where a closed loop can no longer close", "stopped_by_loop" => obj!{"joint" => "Lid stop", "fraction" => 0.0}};
    // Hinge: there and back; Lid: stuck both ways.
    let mut rec = Recorder::new()
        .on("joint.list", Action::Answer(listed), 0)
        .on("joint.drive", Action::Answer(moved.clone()), 2)
        .on("joint.drive", Action::Answer(stuck), 2);
    let design = DesignSteps {
        name: "Box".into(),
        file: "/out/Box.sc".into(),
        steps: vec![
            Step::tool("save", obj! {"path" => "/out/Box.sc"}, "save Box").group("save"),
            Step::tool(
                "verify_joints",
                obj! {"joints" => vec![
                    obj!{"name" => "Hinge", "type" => "revolute", "basis" => "geometry"},
                    obj!{"name" => "Lid", "type" => "revolute", "basis" => "geometry"},
                    obj!{"name" => "Shut", "type" => "revolute", "basis" => "geometry"},
                    obj!{"name" => "Gone", "type" => "revolute", "basis" => "geometry"},
                ]},
                "check the mechanism",
            )
            .soft()
            .group("verification"),
        ],
        notes: Vec::new(),
    };
    let profile = Profile::builtin();
    let r = Runner::new(&mut rec, &profile).run(&design);
    let v = &r.steps[1];
    assert_eq!(v.status, Status::Done, "{:?}", v.status);
    assert!(v.note.contains("1 joint(s) driven, 2 would not move; 1 conflict(s), 0 error(s), 1 floating chain(s)"), "{}", v.note);
    assert!(v.note.contains("1 with 6"), "{}", v.note);
    let checks = v.data.get("joints").and_then(Json::as_array).unwrap().to_vec();
    let of = |n: &str| checks.iter().find(|c| c.str_of("joint") == Some(n)).unwrap().clone();
    assert_eq!(of("Hinge").str_of("result"), Some("moves"));
    assert_eq!(of("Hinge").f64_of("driven_to"), Some(5.0), "5 degrees up, inside its limits");
    assert_eq!(of("Lid").str_of("result"), Some("does not move"));
    assert!(of("Lid").str_of("why").unwrap().contains("the loop through `Lid stop`"), "{:?}", of("Lid"));
    assert_eq!(of("Shut").str_of("result"), Some("no travel within its limits"));
    assert!(of("Gone").str_of("result").unwrap().starts_with("not in the design"));
    // Driven and driven back, then the stuck one tried both ways; names from the profile.
    let drives: Vec<Json> = rec.calls.iter().filter(|(_, a)| a.str_of("command") == Some("joint.drive")).map(|(_, a)| a.get("params").cloned().unwrap()).collect();
    assert_eq!(drives.len(), 4);
    assert_eq!(drives[1].f64_of("value"), Some(0.0), "back where it was built");
    assert_eq!(drives[3].f64_of("value"), Some(-5.0));
    assert_eq!(v.data.get("floating").and_then(Json::as_array).map(<[Json]>::len), Some(1));
}
