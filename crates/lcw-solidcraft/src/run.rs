//! Run build steps through a SolidCraft session, binding the ids SolidCraft hands back.
//!
//! Every step is timed and reported to an [`Observer`] as it finishes (the progress file, the
//! build log and the progress lines on stderr hang off that). Every call has a time limit: a call
//! that doesn't answer in time may still be running in SolidCraft, so its session is given up,
//! the design being built in it is reported as lost, and the next design starts in a fresh
//! session. Independent steps can go to SolidCraft several at a time with its `batch` tool
//! (`batch` > 1); a step that fails there gets the same retries and alternatives as one sent
//! alone.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use lcw_json::{obj, Json};

use crate::mcp::{CallError, Transport};
use crate::profile::{Catalog, Profile};
use crate::steps::{references, substitute, DesignSteps, OnError, Step, STEPS_DIR};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Done,
    /// Done with an alternative op (named), or a retry (described).
    Fallback(String),
    /// Run, but nothing was sent: a value it needs is missing, or there was nothing to do.
    Skipped(String),
    /// SolidCraft refused it (its error), or it got no answer.
    Failed(String),
    /// Never tried, because an earlier step stopped the design or the session was lost.
    NotRun(String),
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Fallback(_) => "fallback",
            Status::Skipped(_) => "skipped",
            Status::Failed(_) => "failed",
            Status::NotRun(_) => "not run",
        }
    }
    pub fn detail(&self) -> &str {
        match self {
            Status::Done => "",
            Status::Fallback(s) | Status::Skipped(s) | Status::Failed(s) | Status::NotRun(s) => s,
        }
    }
    /// It did what it was for (itself or by an alternative).
    pub fn achieved(&self) -> bool {
        matches!(self, Status::Done | Status::Fallback(_))
    }
}

#[derive(Debug, Clone)]
pub struct StepResult {
    pub label: String,
    /// The step's group ("joints", "sheet metal"…).
    pub group: String,
    /// The step's op, or its tool when it has none.
    pub op: String,
    pub status: Status,
    /// What was sent last for it: the SolidCraft command, or the session tool.
    pub command: String,
    /// Time spent on it, all its calls together (ms). In a batch: SolidCraft's own timing of the
    /// step when it gives one, otherwise the batch's time shared out.
    pub ms: f64,
    /// Calls sent for it (a batch counts once for each of its steps).
    pub calls: u32,
    /// It went to SolidCraft in a `batch`.
    pub batched: bool,
    /// Something to know about a step that went through (one end of a limit is a guess…).
    pub note: String,
    /// What a check step found, for the report (`verify_joints`); null otherwise.
    pub data: Json,
}

impl StepResult {
    fn new(step: &Step, status: Status) -> StepResult {
        StepResult { label: step.label.clone(), group: step.group_name().to_string(), op: step.op.clone().unwrap_or_else(|| step.tool.clone()), status, command: String::new(), ms: 0.0, calls: 0, batched: false, note: String::new(), data: Json::Null }
    }
}

#[derive(Debug, Clone)]
pub struct DesignResult {
    pub name: String,
    pub file: String,
    pub saved: bool,
    pub steps: Vec<StepResult>,
    /// Wall time (ms).
    pub ms: f64,
    /// Why the rest of the design was not run, when a step stopped it.
    pub stopped: Option<String>,
    /// The SolidCraft session was given up while this design was built (it was not saved).
    pub session_lost: Option<String>,
    /// The ids bound when it finished (for a later stage on the same design).
    pub vars: BTreeMap<String, Json>,
}

impl DesignResult {
    pub fn failures(&self) -> impl Iterator<Item = &StepResult> {
        self.steps.iter().filter(|s| matches!(s.status, Status::Failed(_)))
    }
    /// How many steps ended with this status name ("done", "failed"…).
    pub fn count(&self, status: &str) -> usize {
        self.steps.iter().filter(|s| s.status.name() == status).count()
    }
    /// "N of M steps done" with what happened to the rest.
    pub fn summary(&self) -> String {
        let achieved = self.steps.iter().filter(|s| s.status.achieved()).count();
        let mut parts = Vec::new();
        for (k, what) in [("fallback", "by a fallback"), ("skipped", "skipped"), ("failed", "failed"), ("not run", "not run")] {
            let n = self.count(k);
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        format!(
            "{achieved} of {} steps done{}, {:.1} s, {}",
            self.steps.len(),
            if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) },
            self.ms / 1000.0,
            if self.saved { "saved" } else { "NOT saved" }
        )
    }
}

/// Told about the run as it goes: progress display, logs.
pub trait Observer {
    fn design_started(&mut self, _design: &DesignSteps) {}
    fn step_finished(&mut self, _design: &DesignSteps, _index: usize, _result: &StepResult) {}
    fn design_finished(&mut self, _result: &DesignResult) {}
    /// Something about the session (a restart…).
    fn event(&mut self, _message: &str) {}
}

/// A batch's settled steps (index, result, lost session), where to go on, and up to where the
/// steps go one at a time.
type BatchOutcome = (Vec<(usize, StepResult, Option<String>)>, usize, Option<usize>);

/// What one step came to.
struct Outcome {
    status: Status,
    note: String,
    data: Json,
    /// The session was given up during it.
    lost: Option<String>,
}

impl Outcome {
    fn of(status: Status) -> Outcome {
        Outcome { status, note: String::new(), data: Json::Null, lost: None }
    }
    fn lost(e: &CallError, what: &str) -> Outcome {
        let msg = e.message().to_string();
        Outcome {
            status: Status::Failed(format!("{msg} (while {what}); the SolidCraft session was given up, since the call may still be running there")),
            note: String::new(),
            data: Json::Null,
            lost: Some(msg),
        }
    }
}

pub struct Runner<'a> {
    pub transport: &'a mut dyn Transport,
    pub profile: &'a Profile,
    pub vars: BTreeMap<String, Json>,
    /// Ids bound before any step runs (the assembly's, for a limits stage run on its own).
    pub preset: BTreeMap<String, Json>,
    /// Print each step as it runs (when there is no observer).
    pub verbose: bool,
    /// The least time any call may take before it is given up (`--timeout`); `None`: the
    /// profile's timeouts.
    pub timeout: Option<Duration>,
    /// The most steps sent in one `batch` call; 0 or 1 sends them one at a time.
    pub batch: usize,
    /// The folder `${steps_dir}` paths are under.
    pub steps_dir: Option<String>,
    pub observer: Option<&'a mut dyn Observer>,
    /// What happened to the session: restarts, batching turned off.
    pub events: Vec<String>,
    session_lost: Option<String>,
    dead: Option<String>,
    batch_off: bool,
    /// The command (or tool) last sent, and the calls sent, for the step running.
    last_command: String,
    calls: u32,
}

impl<'a> Runner<'a> {
    pub fn new(transport: &'a mut dyn Transport, profile: &'a Profile) -> Runner<'a> {
        Runner {
            transport,
            profile,
            vars: BTreeMap::new(),
            preset: BTreeMap::new(),
            verbose: false,
            timeout: None,
            batch: 0,
            steps_dir: None,
            observer: None,
            events: Vec::new(),
            session_lost: None,
            dead: None,
            batch_off: false,
            last_command: String::new(),
            calls: 0,
        }
    }

    /// Read SolidCraft's catalogue and check the profile against it.
    pub fn probe(transport: &mut dyn Transport, profile: &mut Profile) -> Result<(Catalog, Vec<String>), String> {
        let list = transport.call("list_commands", obj! {"limit" => 5000})?;
        let mut catalog = Catalog::from_json(&list);
        if catalog.commands.is_empty() {
            return Err("SolidCraft listed no commands".into());
        }
        if let Ok(v) = transport.call("version", Json::obj()) {
            catalog.version = v.str_of("version").map(str::to_string);
        }
        let commands: Vec<String> = profile.ops.values().map(|o| o.command.clone()).collect();
        for c in commands {
            if catalog.commands.contains_key(&c) {
                if let Ok(d) = transport.call("describe_command", obj! {"id" => c.as_str()}) {
                    catalog.add_schema(&c, &d);
                }
            }
        }
        let report = profile.check(&catalog);
        Ok((catalog, report))
    }

    fn lookup(&self, name: &str) -> Option<Json> {
        self.vars.get(name).cloned()
    }

    fn event(&mut self, msg: String) {
        if let Some(o) = self.observer.as_deref_mut() {
            o.event(&msg);
        } else if self.verbose {
            eprintln!("  {msg}");
        }
        self.events.push(msg);
    }

    /// One MCP tool call with a time limit. A bridged call that outlives the bridge's own wait
    /// (an error starting with the profile's still-running marker) is waited for with the
    /// `pending` tool until the same limit, then cancelled.
    fn send(&mut self, tool: &str, args: Json, timeout: Duration) -> Result<Json, CallError> {
        let deadline = Instant::now() + timeout;
        self.calls += 1;
        match self.transport.call_within(tool, args, timeout) {
            Err(CallError::Failed(e)) => match self.profile.runner.still_running.clone() {
                Some(sr) if e.starts_with(&sr.marker) => self.wait_pending(&sr, e, deadline, timeout),
                _ => Err(CallError::Failed(e)),
            },
            other => other,
        }
    }

    fn wait_pending(&mut self, sr: &crate::profile::StillRunning, first: String, deadline: Instant, timeout: Duration) -> Result<Json, CallError> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            self.calls += 1;
            let v = self.transport.call_within(&sr.pending_tool, Json::obj(), left)?;
            match v.pointer(&sr.done).and_then(Json::as_bool) {
                Some(true) => return Ok(v.pointer(&sr.result).cloned().unwrap_or_default()),
                Some(false) => std::thread::sleep(left.min(Duration::from_millis(20))),
                // Nothing is running any more, and its result is gone: what it did is unknown.
                None => return Err(CallError::Lost(format!("SolidCraft said the call was still running ({first}), then that nothing was running: its result is lost"))),
            }
        }
        let secs = timeout.as_secs_f64().round();
        if let Some(cancel) = sr.cancel_tool.clone() {
            self.calls += 1;
            if self.transport.call_within(&cancel, Json::obj(), Duration::from_secs(30)).is_ok() {
                return Err(CallError::Failed(format!("no result within {secs} s, so it was cancelled in SolidCraft (the design is as it was before it): {first}")));
            }
        }
        Err(CallError::TimedOut(format!("still running in SolidCraft after {secs} s and it could not be cancelled: {first}")))
    }

    fn execute_op(&mut self, op: &str, args: &Json, timeout: Duration) -> Result<Json, CallError> {
        let resolved = self.profile.resolve(op).ok_or_else(|| CallError::Failed(format!("no usable SolidCraft command for `{op}`")))?.to_string();
        if resolved != op {
            // A profile fallback for a whole op cannot take the same parameters; only the step's
            // own alternatives can. Refuse instead of guessing.
            return Err(CallError::Failed(format!("`{op}` is not available in this SolidCraft")));
        }
        let (command, params) = self.profile.call(op, args.clone()).ok_or_else(|| CallError::Failed(format!("unknown op `{op}`")))?;
        self.last_command = command.clone();
        self.send("execute", obj! {"command" => command.as_str(), "params" => params, "verbose" => false}, timeout)
    }

    /// Run one step alone. `known_error`: its op already failed with this (in a batch), so go
    /// straight to its retries and alternatives.
    fn run_step(&mut self, step: &Step, known_error: Option<String>) -> Outcome {
        for n in &step.needs {
            if !self.vars.contains_key(n) {
                return Outcome::of(Status::Skipped(format!("needs `{n}`, which an earlier step did not give")));
            }
        }
        let mut missing = Vec::new();
        let args = substitute(&step.args, &|n| self.lookup(n), &mut missing);
        if !missing.is_empty() {
            return Outcome::of(Status::Skipped(format!("needs {}", missing.join(", "))));
        }
        if step.tool == "auto_limits" {
            return self.auto_limits(&args);
        }
        if step.tool == "verify_joints" {
            return self.verify_joints(&args);
        }
        if step.tool == "joint_limits" {
            return self.joint_limits(&args);
        }
        // Where the guarded placements are before the step.
        let guard: Vec<Json> = step.guard.iter().map(|g| substitute(g, &|n| self.lookup(n), &mut Vec::new())).collect();
        let mut before = Vec::new();
        for g in &guard {
            match self.placement_of(g) {
                Ok(p) => before.push(p),
                Err(o) => return o,
            }
        }
        // The design's volume before a step that must keep it.
        let volume_before = if step.keep_volume {
            match self.volume_now() {
                Ok(v) => v,
                Err(o) => return o,
            }
        } else {
            None
        };
        let timeout = self.profile.timeout_for(step.op.as_deref(), &step.tool, self.timeout);
        let mut first = match (known_error, &step.op) {
            (Some(e), _) => Err(CallError::Failed(e)),
            (None, Some(op)) => self.execute_op(op, &args, timeout),
            (None, None) => {
                self.last_command = step.tool.clone();
                self.send(&step.tool, args.clone(), timeout)
            }
        };
        if let Err(e) = &first {
            if e.ends_session() {
                return Outcome::lost(e, &step.label);
            }
        }
        let mut dropped = Vec::new();
        if let (Err(_), Some(op)) = (&first, &step.op) {
            if let Some(dp) = self.profile.ops.get(op).and_then(|o| o.drop_point.clone()) {
                let mut a = args.clone();
                while let Err(e) = &first {
                    let e = e.message();
                    if !dp.when.iter().any(|w| e.contains(w.as_str())) {
                        break;
                    }
                    let Some(p) = first_point(e) else { break };
                    let Some(list) = a.get(&dp.param).and_then(Json::as_array).map(<[Json]>::to_vec) else { break };
                    let dist = |j: &Json| {
                        let v = j.as_array().unwrap_or(&[]).iter().filter_map(Json::as_f64).collect::<Vec<_>>();
                        if v.len() == 3 { (0..3).map(|i| (v[i] - p[i]).powi(2)).sum::<f64>() } else { f64::INFINITY }
                    };
                    let Some((i, _)) = list.iter().enumerate().map(|(i, j)| (i, dist(j))).min_by(|x, y| x.1.total_cmp(&y.1)) else { break };
                    if list.len() <= 1 {
                        break;
                    }
                    let mut rest = list;
                    dropped.push(rest.remove(i).to_compact());
                    a.set(&dp.param, Json::Arr(rest));
                    first = self.execute_op(op, &a, timeout);
                    if let Err(e) = &first {
                        if e.ends_session() {
                            return Outcome::lost(e, &step.label);
                        }
                    }
                }
                if first.is_err() {
                    dropped.clear();
                }
            }
        }
        let mut note = String::new();
        let (result, status) = match first {
            Ok(r) if !dropped.is_empty() => (r, Status::Fallback(format!("without the bend at {} (cut open there)", dropped.join(", ")))),
            Ok(r) => (r, Status::Done),
            Err(e) => {
                let primary = e.message().to_string();
                let mut tried = Vec::new();
                let mut outcome = None;
                for (alt, alt_args) in &step.alternatives {
                    let mut miss = Vec::new();
                    let a = substitute(alt_args, &|n| self.lookup(n), &mut miss);
                    if !miss.is_empty() {
                        tried.push(format!("`{alt}` needs {}", miss.join(", ")));
                        continue;
                    }
                    let t = self.profile.timeout_for(Some(alt), "execute", self.timeout);
                    match self.execute_op(alt, &a, t) {
                        Ok(r) => {
                            outcome = Some((r, alt.clone()));
                            break;
                        }
                        Err(e) if e.ends_session() => return Outcome::lost(&e, &format!("{}, trying `{alt}` instead", step.label)),
                        Err(e) => tried.push(format!("`{alt}` also failed: {}", e.message())),
                    }
                }
                match outcome {
                    Some((r, alt)) => {
                        // Why the step itself didn't go through is worth keeping.
                        note = format!("`{}` failed first: {primary}", step.op.as_deref().unwrap_or(&step.tool));
                        (r, Status::Fallback(alt))
                    }
                    None if tried.is_empty() => return Outcome::of(Status::Failed(primary)),
                    None => return Outcome::of(Status::Failed(format!("{primary}\n({})", tried.join("; ")))),
                }
            }
        };
        // Placement is the truth: a step that moved a guarded part is undone.
        for (g, b) in guard.iter().zip(&before) {
            let after = match self.placement_of(g) {
                Ok(p) => p,
                Err(o) => return o,
            };
            let moved = b.iter().zip(&after).map(|(x, y)| (0..3).map(|i| (x[i] - y[i]).powi(2)).sum::<f64>().sqrt()).fold(0.0, f64::max);
            let tol = self.profile.mechanism.get("guard").and_then(|x| x.f64_of("tol_mm")).unwrap_or(0.01);
            if b.len() != after.len() || moved > tol {
                let who = g.get("occurrence").map(Json::to_compact).unwrap_or_default();
                let t = self.profile.timeout_for(Some("undo"), "execute", self.timeout);
                let undone = match self.execute_op("undo", &Json::obj(), t) {
                    Ok(_) => match self.placement_of(g) {
                        Ok(p) => p.iter().zip(b).all(|(x, y)| (0..3).all(|i| (x[i] - y[i]).abs() <= tol)),
                        Err(o) => return o,
                    },
                    Err(e) if e.ends_session() => return Outcome::lost(&e, &format!("undoing {}", step.label)),
                    Err(_) => false,
                };
                let why = format!("it moved occurrence {who} {moved:.3} mm from where Inventor has it");
                return Outcome::of(if undone { Status::Skipped(format!("{why}, so it was undone")) } else { Status::Failed(format!("{why}, and undoing it did not put it back")) });
            }
        }
        // A step that must keep the volume (a sheet metal conversion that dropped a flange, or
        // was cut open where it should not be) is undone: the part stays the solid it was.
        if let Some(v0) = volume_before {
            let v1 = match self.volume_now() {
                Ok(v) => v,
                Err(o) => return o,
            };
            let tol = self.profile.mechanism.get("volume_guard").and_then(|x| x.f64_of("tol_rel")).unwrap_or(0.01);
            if let Some(v1) = v1 {
                let rel = (v1 - v0).abs() / v0.abs().max(1e-9);
                if rel > tol {
                    let t = self.profile.timeout_for(Some("undo"), "execute", self.timeout);
                    let undone = match self.execute_op("undo", &Json::obj(), t) {
                        Ok(_) => match self.volume_now() {
                            Ok(Some(v)) => (v - v0).abs() / v0.abs().max(1e-9) <= tol,
                            Ok(None) => false,
                            Err(o) => return o,
                        },
                        Err(e) if e.ends_session() => return Outcome::lost(&e, &format!("undoing {}", step.label)),
                        Err(_) => false,
                    };
                    let how = match &status {
                        Status::Fallback(h) => format!(" ({h})"),
                        _ => String::new(),
                    };
                    let why = format!("it changed the part's volume by {:.1}% ({} to {} mm3){how}", rel * 100.0, fmt3(v0), fmt3(v1));
                    return Outcome::of(Status::Failed(if undone { format!("{why}, so it was undone and the part stays a solid") } else { format!("{why}, and undoing it did not restore the volume") }));
                }
            }
        }
        self.bind(step, &result);
        Outcome { status, note, data: Json::Null, lost: None }
    }

    /// The design's closed-solid volume (mm3), as the profile's `mechanism.volume_guard` reads
    /// it; None when it cannot be read (then nothing is guarded).
    fn volume_now(&mut self) -> Result<Option<f64>, Outcome> {
        let vg = self.profile.mechanism.get("volume_guard").cloned().unwrap_or_default();
        let (Some(op), Some(ptr)) = (vg.str_of("op").map(str::to_string), vg.str_of("volume").map(str::to_string)) else { return Ok(None) };
        let t = self.profile.timeout_for(Some(&op), "execute", self.timeout);
        match self.execute_op(&op, &Json::obj(), t) {
            Ok(r) => Ok(r.pointer(&ptr).and_then(Json::as_f64).filter(|v| v.is_finite() && *v > 0.0)),
            Err(e) if e.ends_session() => Err(Outcome::lost(&e, "measuring the design's volume")),
            Err(_) => Ok(None),
        }
    }

    /// A placement's position, as the points the profile's `mechanism.guard` names (centre of
    /// mass, box corners) read from `occurrence_props`.
    fn placement_of(&mut self, g: &Json) -> Result<Vec<[f64; 3]>, Outcome> {
        let t = self.profile.timeout_for(Some("occurrence_props"), "execute", self.timeout);
        let r = match self.execute_op("occurrence_props", g, t) {
            Ok(r) => r,
            Err(e) if e.ends_session() => return Err(Outcome::lost(&e, "reading a placement")),
            Err(e) => return Err(Outcome::of(Status::Skipped(format!("not run: its parts' placement could not be read to check it ({})", e.message())))),
        };
        let guard = self.profile.mechanism.get("guard").cloned().unwrap_or_default();
        Ok(guard
            .arr_of("points")
            .iter()
            .filter_map(|p| p.as_str())
            .filter_map(|p| {
                let a = r.pointer(p)?.as_array()?;
                (a.len() == 3).then(|| [a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0), a[2].as_f64().unwrap_or(0.0)])
            })
            .collect())
    }

    fn bind(&mut self, step: &Step, result: &Json) {
        for (name, pointers) in &step.bind {
            if let Some(v) = pointers.iter().find_map(|p| result.pointer(p).filter(|v| !v.is_null()).cloned()) {
                self.vars.insert(name.clone(), v);
            }
        }
    }

    /// Set a joint's limits from `joint.find_limits`, unless nothing stops it (a full turn).
    /// A limit only partly known is not set, and the step says which end is unknown and why.
    fn auto_limits(&mut self, args: &Json) -> Outcome {
        let found = args.get("found").cloned().unwrap_or_default();
        let joint = args.get("joint").cloned().unwrap_or_default();
        let jname = joint.as_str().map(str::to_string).unwrap_or_else(|| joint.to_compact());
        if found.get("min").is_none() && found.get("max").is_none() {
            return Outcome::of(Status::Skipped(format!("limit not set: SolidCraft's answer for {jname} has no min or max ({})", clip(&found.to_compact(), 300))));
        }
        let side = |k: &str| -> (Option<f64>, bool, String) {
            let value = found.f64_of(k);
            let by = found.get(&format!("{k}_by")).cloned().unwrap_or_default();
            let (stops, why) = match &by {
                Json::Str(s) if s == "turn" => (false, "no contact within a turn".to_string()),
                Json::Str(s) if s == "range" => (false, "no contact within the parts' reach".to_string()),
                // The last value checked clear is not a stop: as a limit it would hold the joint
                // there (a lever left a few degrees of its travel).
                Json::Str(s) if s == "budget" => (false, "the search ran out of time before finding the stop".to_string()),
                // The closed loop the joint is in stops it; a limit of its own would lock the loop
                // where the sweep of this one joint met it.
                Json::Str(s) if s == "loop" => (false, "the closed loop it is in stops it, so it needs no limit of its own".to_string()),
                // Only a contact is a stop: anything else (another joint's limit…) is not this joint's.
                Json::Str(s) => (false, format!("stopped by {s}, not by a contact")),
                Json::Obj(_) => {
                    let names: Vec<&str> = ["a", "b"].iter().filter_map(|n| by.str_of(n)).collect();
                    (true, if names.len() == 2 { format!("stopped where {} meets {}", names[0], names[1]) } else { "stopped by contact".to_string() })
                }
                Json::Arr(_) => (true, "stopped by contact".to_string()),
                _ => (false, "no reason given".to_string()),
            };
            let text = match value {
                Some(v) => format!("{k} {v} ({why})"),
                None => format!("{k} unknown ({why})"),
            };
            // Only a real stop makes an end of a limit.
            (value.filter(|_| stops), stops, text)
        };
        let (min, min_stops, min_text) = side("min");
        let (max, max_stops, max_text) = side("max");
        let (Some(lo), Some(hi)) = (min, max) else {
            if !min_stops && !max_stops {
                return Outcome::of(Status::Skipped(format!("nothing stops it either way: {min_text}; {max_text}")));
            }
            return Outcome::of(Status::Skipped(format!("limit not set, one end unknown: {min_text}; {max_text} (SolidCraft's joint limits need both ends)")));
        };
        if !(min_stops || max_stops) {
            return Outcome::of(Status::Skipped(format!("it turns freely: nothing stops it ({min_text}; {max_text})")));
        }
        if hi - lo < 1e-6 {
            return Outcome::of(Status::Skipped(format!("limit not set: the travel found is empty ({min_text}; {max_text})")));
        }
        let t = self.profile.timeout_for(Some("set_limits"), "execute", self.timeout);
        let r = self.execute_op("set_limits", &obj! {"joint" => joint, "index" => 0, "min" => lo, "max" => hi}, t);
        match r {
            Ok(_) => Outcome::of(Status::Done),
            Err(e) if e.ends_session() => Outcome::lost(&e, &format!("setting the limits of {jname}")),
            Err(e) => Outcome::of(Status::Failed(e.message().to_string())),
        }
    }

    /// The limits stage's search for one joint (`joint_limits`): its value swept with the `sweep`
    /// op from where it is, up by `range` (a turn, or the parts' reach) and then down by as much
    /// from where that stopped, the moving parts (`occurrences`) checked against the parts they
    /// can meet (`against`), with `budget_s` each way. A limit is set only where both ways end
    /// at a contact; every other ending is reported as what it is: nothing stops it (a full turn,
    /// or the whole reach), not found in time, or a closed loop stops it. Each result is appended
    /// to `record` (a JSON-lines file) as it is found and the design saved to `save` after a limit
    /// is set, so a stage cut short is taken up from there: a joint already in the record is not
    /// searched again (one not found in time is).
    fn joint_limits(&mut self, args: &Json) -> Outcome {
        let lp = self.profile.limits.clone();
        let key = |k: &str| lp.str_of(k).unwrap_or(k).to_string();
        let joint = args.get("joint").cloned().unwrap_or_default();
        let jname = joint.as_str().map(str::to_string).unwrap_or_else(|| joint.to_compact());
        let unit = args.str_of("unit").unwrap_or("deg").to_string();
        let record = args.str_of("record").map(str::to_string);
        let t0 = Instant::now();
        // Taken up from an earlier run of the stage.
        if let Some(prev) = record.as_deref().and_then(|r| earlier_result(r, &jname)) {
            let result = prev.str_of("result").unwrap_or("").to_string();
            let text = prev.str_of("text").unwrap_or("").to_string();
            if result == "contact" {
                if let (Some(lo), Some(hi)) = (prev.f64_of("min"), prev.f64_of("max")) {
                    let t = self.profile.timeout_for(Some("set_limits"), "execute", self.timeout);
                    return match self.execute_op("set_limits", &obj! {"joint" => joint.clone(), "index" => 0, "min" => lo, "max" => hi}, t) {
                        Ok(_) => Outcome { status: Status::Done, note: format!("{text} (found by an earlier run of the limits stage)"), data: prev.clone(), lost: None },
                        Err(e) if e.ends_session() => Outcome::lost(&e, &format!("setting the limits of {jname}")),
                        Err(e) => Outcome::of(Status::Failed(e.message().to_string())),
                    };
                }
            } else if result != "not found" && result != "error" && !result.is_empty() {
                return Outcome { status: Status::Skipped(format!("{text} (found by an earlier run of the limits stage)")), note: String::new(), data: prev.clone(), lost: None };
            }
        }
        let finish = |runner: &mut Runner, data: Json, status: Status| -> Outcome {
            if let Some(r) = &record {
                let mut line = data.clone();
                line.set("seconds", (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
                let _ = append_line(r, &line);
            }
            let mut o = Outcome { status, note: String::new(), data, lost: None };
            if o.status == Status::Done {
                if let Some(path) = args.str_of("save") {
                    let t = runner.profile.timeout_for(None, "save", runner.timeout);
                    if let Err(e) = runner.send("save", obj! {"path" => path}, t) {
                        if e.ends_session() {
                            return Outcome::lost(&e, &format!("saving after the limits of {jname}"));
                        }
                        o.note = format!("the design could not be saved after this limit: {}", e.message());
                    }
                }
            }
            o
        };
        let movers = args.arr_of("occurrences").to_vec();
        let against = args.arr_of("against").to_vec();
        let base = obj! {"joint" => jname.as_str(), "unit" => unit.as_str()};
        if movers.is_empty() || against.is_empty() {
            let mut d = base.clone();
            d.set("result", "free");
            d.set("text", "nothing it can meet anywhere on its travel, so nothing stops it: no limit");
            return finish(self, d, Status::Skipped("nothing it can meet anywhere on its travel, so nothing stops it: no limit".into()));
        }
        // Where the joint is now: the searches start there.
        let mp = self.profile.mechanism.get("list").cloned().unwrap_or_default();
        let mkey = |k: &str| mp.str_of(k).unwrap_or(k).to_string();
        let t = self.profile.timeout_for(Some("joint_list"), "execute", self.timeout);
        let now = match self.execute_op("joint_list", &obj! {"quiet" => false}, t) {
            Ok(r) => r
                .pointer(&mkey("joints"))
                .and_then(Json::as_array)
                .and_then(|l| l.iter().find(|x| x.str_of(&mkey("joint_name")) == Some(jname.as_str())).cloned())
                .and_then(|x| x.get(&mkey("joint_values")).and_then(Json::as_array).and_then(|a| a.first()).and_then(|v| v.f64_of(&mkey("value"))))
                .unwrap_or(0.0),
            Err(e) if e.ends_session() => return Outcome::lost(&e, "listing the joints"),
            Err(_) => 0.0,
        };
        // A limit of its own (from an earlier search) would stop the sweeps: cleared first. Only
        // joints with no limit from Inventor are searched, so any limit there is a search's.
        let t = self.profile.timeout_for(Some("set_limits"), "execute", self.timeout);
        if let Err(e) = self.execute_op("set_limits", &obj! {"joint" => joint.clone(), "index" => 0}, t) {
            if e.ends_session() {
                return Outcome::lost(&e, &format!("clearing the limits of {jname}"));
            }
        }
        let range = args.f64_of("range").unwrap_or(360.0);
        let budget = args.f64_of("budget_s").unwrap_or(30.0);
        let slack = lp.f64_of("slack_s").unwrap_or(120.0);
        let timeout = self.profile.timeout_for(Some("sweep"), "execute", self.timeout).max(Duration::from_secs_f64(budget + slack));
        // One sweep: what ended it, and its reply.
        let sweep = |runner: &mut Runner, occ: &[Json], against: &[Json], exclude: &[Json], step: Option<Json>, to: f64| -> Result<(End, Json), Outcome> {
            let mut p = obj! {"joint" => joint.clone(), "index" => 0, "from" => now, "to" => to, "occurrences" => Json::Arr(occ.to_vec()), "against" => Json::Arr(against.to_vec()), "budget_s" => budget, "apply" => false};
            if !exclude.is_empty() {
                p.set("exclude_pairs", Json::Arr(exclude.to_vec()));
            }
            if let Some(st) = step {
                p.set("step", st);
            }
            if let Some(v) = args.get("refine").filter(|v| !v.is_null()) {
                p.set("refine", v.clone());
            }
            match runner.execute_op("sweep", &p, timeout) {
                Ok(r) => {
                    let at = r.pointer(&key("stopped_at")).and_then(Json::as_f64);
                    let by = r.pointer(&key("stopped_by")).cloned().unwrap_or(Json::Null);
                    let ran = r.pointer(&key("ran_out")).and_then(Json::as_str).map(str::to_string);
                    let end = match (&by, at) {
                        (Json::Obj(_), Some(v)) => {
                            let pt = by.get(&key("contact_at")).and_then(Json::as_array).map(|a| a.iter().filter_map(Json::as_f64).map(|x| (x * 100.0).round() / 100.0).collect::<Vec<_>>()).unwrap_or_default();
                            End::Contact { at: v, a: by.str_of(&key("contact_a")).unwrap_or("?").to_string(), b: by.str_of(&key("contact_b")).unwrap_or("?").to_string(), point: pt }
                        }
                        (Json::Str(s), v) if *s == key("budget") => End::Budget { at: v, ran_out: ran.unwrap_or_else(|| "work".into()) },
                        (Json::Str(s), v) if *s == key("loop") => End::Other { at: v, why: "the closed loop it is in stops it".into() },
                        (Json::Str(s), v) if *s == key("limit") => End::Other { at: v, why: "another joint's limit stops it".into() },
                        (Json::Str(s), v) => End::Other { at: v, why: format!("stopped by {s}") },
                        (Json::Null, _) => End::Clear,
                        _ => End::Other { at, why: format!("an answer it does not understand ({})", clip(&by.to_compact(), 120)) },
                    };
                    Ok((end, r))
                }
                Err(e) if e.ends_session() => Err(Outcome::lost(&e, &format!("searching the travel of {jname}"))),
                Err(e) => Ok((End::Error(e.message().to_string()), Json::Null)),
            }
        };
        let step = args.get("step").filter(|v| !v.is_null()).cloned();
        let rest_step = args.get("rest_step").filter(|v| !v.is_null()).cloned().or_else(|| step.clone());
        // Parts already touching where it starts (a lever on its bracket, a pin in a hole) stop it
        // only by passing into each other deeper than they start, and measuring that at every
        // sample is most of a search's work. So each way is searched in two sweeps: every other
        // pair at the fine step, then the touching pairs alone at `rest_step`, only as far as the
        // first stopped (its travel since the touch is continuous from contact, so a coarser
        // step does not jump over it). The earlier stop of the two ends that way.
        let first = args.f64_of("step").unwrap_or(1.0).max(1e-6);
        let (_, probe) = match sweep(self, &movers, &against, &[], step.clone(), now + first) {
            Ok(x) => x,
            Err(o) => return o,
        };
        let ids = |list: &[Json]| -> Vec<u64> { list.iter().filter_map(Json::as_u64).collect() };
        let (mover_ids, against_ids) = (ids(&movers), ids(&against));
        let mut resting: Vec<(u64, u64)> = Vec::new();
        for pair in probe.pointer(&key("touching_parts")).and_then(Json::as_array).unwrap_or(&[]) {
            let member = |side: Option<&Json>, set: &[u64]| -> Option<u64> { side?.get(&key("path_ids"))?.as_array()?.iter().filter_map(Json::as_u64).rev().find(|id| set.contains(id)) };
            let (x, y) = (pair.as_array().and_then(|p| p.first()), pair.as_array().and_then(|p| p.get(1)));
            let pick = match (member(x, &mover_ids), member(y, &against_ids), member(y, &mover_ids), member(x, &against_ids)) {
                (Some(m), Some(o), _, _) | (_, _, Some(m), Some(o)) => Some((m, o)),
                _ => None,
            };
            if let Some(p) = pick {
                if !resting.contains(&p) {
                    resting.push(p);
                }
            }
        }
        let exclude: Vec<Json> = resting.iter().map(|(m, o)| Json::Arr(vec![Json::from(*m as f64), Json::from(*o as f64)])).collect();
        let mut rest_movers: Vec<Json> = Vec::new();
        let mut rest_against: Vec<Json> = Vec::new();
        for (m, o) in &resting {
            if !rest_movers.iter().any(|x| x.as_u64() == Some(*m)) {
                rest_movers.push(Json::from(*m as f64));
            }
            if !rest_against.iter().any(|x| x.as_u64() == Some(*o)) {
                rest_against.push(Json::from(*o as f64));
            }
        }
        let mut sweeps = 1usize;
        let mut side = |runner: &mut Runner, to: f64| -> Result<End, Outcome> {
            // Every pair but the touching ones (when every pair touches, there is nothing to do here).
            let all_resting = against_ids.iter().all(|o| resting.iter().any(|(_, x)| x == o)) && mover_ids.iter().all(|m| resting.iter().any(|(x, _)| x == m)) && resting.len() == mover_ids.len() * against_ids.len();
            let a = if all_resting {
                End::Clear
            } else {
                sweeps += 1;
                sweep(runner, &movers, &against, &exclude, step.clone(), to)?.0
            };
            if resting.is_empty() {
                return Ok(a);
            }
            let until = match &a {
                End::Contact { at, .. } => *at,
                End::Budget { at: Some(v), .. } | End::Other { at: Some(v), .. } => *v,
                End::Error(_) => return Ok(a),
                _ => to,
            };
            if (until - now).abs() < 1e-9 {
                return Ok(a);
            }
            sweeps += 1;
            let b = sweep(runner, &rest_movers, &rest_against, &[], rest_step.clone(), until)?.0;
            Ok(match b {
                End::Contact { .. } | End::Error(_) => b,
                // The touching pairs not checked all the way: not found that far.
                End::Budget { at, ran_out } => match a {
                    End::Budget { at: aa, .. } => End::Budget { at: match (at, aa) { (Some(x), Some(y)) => Some(if to > now { x.min(y) } else { x.max(y) }), (x, y) => x.or(y) }, ran_out },
                    _ => End::Budget { at, ran_out },
                },
                _ => a,
            })
        };
        let up = match side(self, now + range) {
            Ok(e) => e,
            Err(o) => return o,
        };
        // Down from where it is, as far as a turn below where the way up stopped (so a span of at
        // most a turn), or the whole reach.
        let down_to = match (&up, unit.as_str()) {
            (End::Contact { at, .. }, "deg") => at - range,
            _ => now - range,
        };
        // A whole turn up with nothing met has been through every pose the way down would see.
        let down = if matches!(up, End::Clear) && unit == "deg" && range >= 360.0 - 1e-9 {
            End::Clear
        } else {
            match side(self, down_to) {
                Ok(e) => e,
                Err(o) => return o,
            }
        };
        drop(side);
        let way = |e: &End, w: &str| -> String {
            match e {
                End::Contact { at, a, b, point } => format!("{w} {} {unit}: {a} meets {b}{}", fmt3(*at), if point.len() == 3 { format!(" at ({}, {}, {})", point[0], point[1], point[2]) } else { String::new() }),
                End::Clear => format!("{w}: nothing met {}", if unit == "deg" { "within a turn" } else { "within the parts' reach" }),
                End::Budget { at, ran_out } => format!("{w}: not found ({} budget ran out{})", ran_out, at.map(|v| format!("; clear to {} {unit}", fmt3(v))).unwrap_or_default()),
                End::Other { at, why } => format!("{w}: {why}{}", at.map(|v| format!(" at {} {unit}", fmt3(v))).unwrap_or_default()),
                End::Error(e) => format!("{w}: the search failed ({})", clip(e, 200)),
            }
        };
        let text_of = |head: &str| format!("{head} ({}; {})", way(&down, "min"), way(&up, "max"));
        let mut d = base.clone();
        d.set("touching_pairs", resting.len());
        d.set("sweeps", sweeps);
        d.set("min_by", down.json());
        d.set("max_by", up.json());
        let (result, status) = match (&down, &up) {
            // Less than 2 (degrees or mm) between the stops means the part is jammed where it
            // sits: a clash in the model, or parts wrongly grouped with it. Setting that as a
            // limit would lock a part that should move, so it is reported and left free.
            (End::Contact { at: lo, .. }, End::Contact { at: hi, .. }) if hi - lo >= 2.0 => {
                d.set("min", *lo);
                d.set("max", *hi);
                let t = self.profile.timeout_for(Some("set_limits"), "execute", self.timeout);
                match self.execute_op("set_limits", &obj! {"joint" => joint.clone(), "index" => 0, "min" => *lo, "max" => *hi}, t) {
                    Ok(_) => ("contact", Status::Done),
                    Err(e) if e.ends_session() => return Outcome::lost(&e, &format!("setting the limits of {jname}")),
                    Err(e) => ("error", Status::Failed(format!("{}: {}", text_of("both ways stop at a contact, but setting the limits failed"), e.message()))),
                }
            }
            (End::Contact { .. }, End::Contact { .. }) => ("jammed", Status::Skipped(text_of("limit not set: under 2 of travel between the two contacts, so the part is jammed where it sits (a clash in the model, or parts wrongly grouped with it)"))),
            (End::Clear, End::Clear) => ("free", Status::Skipped(text_of(if unit == "deg" { "a full turn: nothing stops it, no limit" } else { "nothing stops it within its reach: no limit" }))),
            (End::Error(_), _) | (_, End::Error(_)) => ("error", Status::Failed(text_of("the search failed"))),
            (End::Budget { .. }, _) | (_, End::Budget { .. }) => ("not found", Status::Skipped(text_of("not found in time: no limit set"))),
            (End::Other { .. }, _) | (_, End::Other { .. }) => ("loop", Status::Skipped(text_of("not stopped by a contact both ways: no limit set"))),
            _ => ("one side", Status::Skipped(text_of("a contact one way only: no limit set"))),
        };
        d.set("result", result);
        let text = match &status {
            Status::Done => text_of("contact stop both ways: limits set"),
            st => st.detail().to_string(),
        };
        d.set("text", text.as_str());
        let mut o = finish(self, d, status);
        if o.status == Status::Done && o.note.is_empty() {
            o.note = text;
        }
        o
    }

    /// Check the mechanism SolidCraft solved: degrees of freedom, conflicts and floating chains
    /// from `joint_list`, then each joint of `joints` (`{name, type, basis}`) driven a little
    /// within its limits and back. Every name it reads is in the profile's `mechanism` section.
    fn verify_joints(&mut self, args: &Json) -> Outcome {
        let v = self.profile.mechanism.clone();
        let (Some(lp), Some(dp)) = (v.get("list").cloned(), v.get("drive").cloned()) else {
            return Outcome::of(Status::Skipped("the profile has no `mechanism` section".into()));
        };
        let key = |j: &Json, k: &str| j.str_of(k).unwrap_or(k).to_string();
        let at = |r: &Json, sec: &Json, k: &str| -> Json { r.pointer(&key(sec, k)).cloned().unwrap_or(Json::Null) };
        let t = self.profile.timeout_for(Some("joint_list"), "execute", self.timeout);
        let list = match self.execute_op("joint_list", &obj! {"quiet" => false}, t) {
            Ok(r) => r,
            Err(e) if e.ends_session() => return Outcome::lost(&e, "listing the joints"),
            Err(e) => return Outcome::of(Status::Failed(e.message().to_string())),
        };
        // Degrees of freedom left per occurrence.
        let mut dof: BTreeMap<i64, usize> = BTreeMap::new();
        for d in at(&list, &lp, "dof").as_array().unwrap_or(&[]) {
            if let Some(n) = d.get(&key(&lp, "dof_value")).and_then(Json::as_f64) {
                *dof.entry(n as i64).or_default() += 1;
            }
        }
        let problems = |which: &str| -> Vec<Json> {
            at(&list, &lp, which).as_array().unwrap_or(&[]).iter().map(|p| obj! {"joint" => p.str_of(&key(&lp, "problem_name")).unwrap_or("?"), "why" => p.str_of(&key(&lp, "problem_why")).unwrap_or("")}).collect()
        };
        let conflicts = problems("conflicts");
        let errors = problems("errors");
        let floating: Vec<Json> = at(&list, &lp, "floating")
            .as_array()
            .unwrap_or(&[])
            .iter()
            .map(|c| obj! {"occurrences" => c.get(&key(&lp, "chain_occurrences")).cloned().unwrap_or(Json::Null), "joints" => c.get(&key(&lp, "chain_joints")).cloned().unwrap_or(Json::Null)})
            .collect();
        let listed = at(&list, &lp, "joints");
        let listed = listed.as_array().unwrap_or(&[]);
        let types: Vec<String> = v.arr_of("drive_types").iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
        let step_of = |unit: &str| v.get("test_move").and_then(|m| m.f64_of(if unit == "mm" { "mm" } else { "deg" })).unwrap_or(if unit == "mm" { 1.0 } else { 5.0 });
        let mut checks = Vec::new();
        let (mut driven, mut stuck) = (0usize, 0usize);
        for j in args.arr_of("joints") {
            let name = j.str_of("name").unwrap_or("").to_string();
            let kind = j.str_of("type").unwrap_or("").to_string();
            let mut c = obj! {"joint" => name.as_str(), "type" => kind.as_str(), "basis" => j.str_of("basis").unwrap_or("")};
            let found = listed.iter().find(|x| x.str_of(&key(&lp, "joint_name")) == Some(name.as_str()));
            let Some(found) = found else {
                c.set("result", "not in the design (its step failed or was left out)");
                checks.push(c);
                continue;
            };
            let first = found.get(&key(&lp, "joint_values")).and_then(Json::as_array).and_then(|a| a.first()).cloned().unwrap_or_default();
            let v0 = first.f64_of(&key(&lp, "value")).unwrap_or(0.0);
            let unit = first.str_of(&key(&lp, "value_unit")).unwrap_or("deg").to_string();
            let lim = first.get(&key(&lp, "value_limits")).and_then(Json::as_array).filter(|l| l.len() == 2).map(|l| [l[0].as_f64().unwrap_or(f64::NEG_INFINITY), l[1].as_f64().unwrap_or(f64::INFINITY)]);
            if let Some(l) = lim.filter(|l| l[0].is_finite() && l[1].is_finite()) {
                c.set("limits", Json::from(l));
            }
            c.set("unit", unit.as_str());
            if !types.contains(&kind) {
                c.set("result", "not driven (no motion to check)");
                checks.push(c);
                continue;
            }
            let step = step_of(&unit);
            let (up, down) = match lim {
                Some([lo, hi]) => (hi - v0, v0 - lo),
                None => (f64::INFINITY, f64::INFINITY),
            };
            let mut tries: Vec<f64> = Vec::new();
            for room in [(up, 1.0), (down, -1.0)] {
                if room.0 > 1e-6 {
                    tries.push(v0 + room.1 * step.min(room.0 / 2.0));
                }
            }
            if tries.is_empty() {
                c.set("result", "no travel within its limits");
                stuck += 1;
                checks.push(c);
                continue;
            }
            let dt = self.profile.timeout_for(Some("drive"), "execute", self.timeout);
            let mut outcome = None;
            let mut why_not = Vec::new();
            for target in tries {
                match self.execute_op("drive", &obj! {"joint" => name.as_str(), "value" => target, "quiet" => true}, dt) {
                    Ok(r) => {
                        let moved = at(&r, &dp, "moved").as_bool().unwrap_or(false);
                        let reached = at(&r, &dp, "values").as_array().and_then(|a| a.first()).and_then(|x| x.f64_of(&key(&lp, "value")));
                        if moved {
                            outcome = Some((target, reached, at(&r, &dp, "stopped_by_loop"), at(&r, &dp, "stopped_by_contact")));
                            break;
                        }
                        let mut w = at(&r, &dp, "reason").as_str().unwrap_or("it did not move").to_string();
                        if let Some(l) = at(&r, &dp, "stopped_by_loop").str_of("joint") {
                            w.push_str(&format!(" (the loop through `{l}`)"));
                        }
                        why_not.push(format!("to {target:.3} {unit}: {w}"));
                    }
                    Err(e) if e.ends_session() => return Outcome::lost(&e, &format!("driving {name}")),
                    Err(e) => why_not.push(format!("to {target:.3} {unit}: {}", e.message())),
                }
            }
            match outcome {
                Some((target, reached, by_loop, by_contact)) => {
                    driven += 1;
                    c.set("result", "moves");
                    c.set("driven_to", target);
                    if let Some(x) = reached {
                        c.set("reached", (x * 1e4).round() / 1e4);
                    }
                    if !by_loop.is_null() {
                        c.set("stopped_by_loop", by_loop);
                    }
                    if !by_contact.is_null() {
                        c.set("stopped_by_contact", by_contact);
                    }
                    // Back to where it was built.
                    let _ = self.execute_op("drive", &obj! {"joint" => name.as_str(), "value" => v0, "quiet" => true}, dt);
                }
                None => {
                    stuck += 1;
                    c.set("result", "does not move");
                    c.set("why", why_not.join("; "));
                }
            }
            checks.push(c);
        }
        let dof_text: Vec<String> = dof.iter().map(|(d, n)| format!("{n} with {d}")).collect();
        let note = format!(
            "{driven} joint(s) driven, {stuck} would not move; {} conflict(s), {} error(s), {} floating chain(s); occurrences by degrees of freedom left: {}",
            conflicts.len(),
            errors.len(),
            floating.len(),
            if dof_text.is_empty() { "none listed".to_string() } else { dof_text.join(", ") }
        );
        let dof_json = {
            let mut o = Json::obj();
            for (d, n) in &dof {
                o.set(&d.to_string(), *n as f64);
            }
            o
        };
        let fixed = at(&list, &lp, "fixed").as_array().map_or(0, <[Json]>::len);
        let data = obj! {"dof" => dof_json, "fixed" => fixed as f64, "conflicts" => Json::Arr(conflicts), "errors" => Json::Arr(errors), "floating" => Json::Arr(floating), "joints" => Json::Arr(checks)};
        Outcome { status: Status::Done, note, data, lost: None }
    }

    fn batching(&self) -> bool {
        self.batch > 1 && !self.batch_off && self.profile.runner.batch.is_some()
    }

    /// The steps from `start` that can go to SolidCraft in one batch: ops whose values are all
    /// known before the batch starts (none uses what an earlier one in the batch gives).
    fn batch_group(&self, design: &DesignSteps, start: usize) -> Vec<(usize, String, Json)> {
        let mut group = Vec::new();
        let mut given: BTreeSet<&str> = BTreeSet::new();
        for (i, step) in design.steps.iter().enumerate().skip(start) {
            if group.len() >= self.batch {
                break;
            }
            let Some(op) = step.op.as_deref() else { break };
            if !step.guard.is_empty() || step.keep_volume {
                break;
            }
            if step.tool != "execute" || self.profile.resolve(op) != Some(op) {
                break;
            }
            let mut refs = step.needs.clone();
            references(&step.args, &mut refs);
            if refs.iter().any(|r| given.contains(r.as_str()) || !self.vars.contains_key(r)) {
                break;
            }
            let mut missing = Vec::new();
            let args = substitute(&step.args, &|n| self.lookup(n), &mut missing);
            let Some((command, params)) = self.profile.call(op, args) else { break };
            if !missing.is_empty() {
                break;
            }
            given.extend(step.bind.iter().map(|(n, _)| n.as_str()));
            group.push((i, command, params));
        }
        group
    }

    /// Send a batch. Returns the outcomes of the steps it settled (from the first, in order)
    /// and where to go on: steps after those were not run and are taken up again.
    fn run_batch(&mut self, design: &DesignSteps, group: &[(usize, String, Json)]) -> BatchOutcome {
        let Some(bp) = self.profile.runner.batch.clone() else { return (Vec::new(), group[0].0, Some(group[0].0 + 1)) };
        let mut args = bp.args.clone();
        args.set(
            &bp.commands,
            Json::Arr(group.iter().map(|(_, c, p)| {
                let mut s = Json::obj();
                s.set(&bp.step_command, c.as_str());
                s.set(&bp.step_params, p.clone());
                s
            }).collect()),
        );
        let timeout: Duration = group.iter().map(|(i, _, _)| self.profile.timeout_for(design.steps[*i].op.as_deref(), "execute", self.timeout)).sum();
        let t0 = Instant::now();
        self.calls = 0;
        let reply = self.send(&bp.tool, args, timeout);
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let labels = || group.iter().map(|(i, _, _)| design.steps[*i].label.as_str()).take(4).collect::<Vec<_>>().join("; ");
        let mut out = Vec::new();
        let timing_of = |v: &Json| -> BTreeMap<usize, f64> {
            if bp.timings.is_empty() {
                return BTreeMap::new();
            }
            v.pointer(&bp.timings).and_then(Json::as_array).unwrap_or(&[]).iter().filter_map(|r| Some((r.get(&bp.timing_index)?.as_u64()? as usize, r.get(&bp.timing_ms)?.as_f64()?))).collect()
        };
        let mut settle = |runner: &mut Runner, k: usize, result: Option<&Json>, ms: Option<f64>, n: usize| {
            let (i, command, _) = &group[k];
            let step = &design.steps[*i];
            if let Some(r) = result {
                runner.bind(step, r);
            }
            let mut sr = StepResult::new(step, Status::Done);
            sr.command = command.clone();
            sr.ms = ms.unwrap_or(total_ms / n.max(1) as f64);
            sr.calls = 1;
            sr.batched = true;
            out.push((*i, sr, None));
        };
        match reply {
            Ok(v) => {
                let results = v.pointer(&bp.results).and_then(Json::as_array).map(<[Json]>::to_vec).unwrap_or_default();
                let times = timing_of(&v);
                for k in 0..group.len() {
                    settle(self, k, results.get(k), times.get(&k).copied(), group.len());
                }
                let next = group.last().map_or(0, |g| g.0 + 1);
                (out, next, None)
            }
            Err(CallError::Failed(e)) => {
                let parsed = Json::parse(&e).ok().filter(|r| r.pointer(&bp.failed_index).and_then(Json::as_u64).is_some());
                let Some(r) = parsed else {
                    // SolidCraft didn't say which steps ran: the design's state is unknown, so this
                    // session can't be trusted with the rest of it.
                    self.batch_off = true;
                    self.event(format!("batching turned off: a batch failed without saying which of its steps ran ({})", clip(&e, 200)));
                    let msg = format!("a batch of {} steps ({}…) failed and SolidCraft did not say which of them ran: {e}", group.len(), labels());
                    self.session_lost = Some(msg.clone());
                    let (i, command, _) = &group[0];
                    let mut sr = StepResult::new(&design.steps[*i], Status::Failed(format!("{msg}; the SolidCraft session was given up, since the design's state is unknown")));
                    sr.command = command.clone();
                    sr.ms = total_ms;
                    sr.calls = 1;
                    sr.batched = true;
                    out.push((*i, sr, Some(msg)));
                    return (out, group[0].0 + 1, None);
                };
                let results = r.pointer(&bp.results).and_then(Json::as_array).map(<[Json]>::to_vec).unwrap_or_default();
                let rolled_back = r.pointer(&bp.rolled_back).and_then(Json::as_bool) == Some(true);
                let completed = if rolled_back { 0 } else { results.len().min(group.len()) };
                let failed = r.pointer(&bp.failed_index).and_then(Json::as_u64).unwrap_or(0) as usize;
                let error = r.pointer(&bp.failed_error).and_then(Json::as_str).unwrap_or(&e).to_string();
                let times = timing_of(&r);
                for k in 0..completed {
                    settle(self, k, results.get(k), times.get(&k).copied(), group.len());
                }
                if failed == completed && failed < group.len() {
                    // The step that failed gets its retries and alternatives, alone.
                    let (i, command, _) = &group[failed];
                    let step = &design.steps[*i];
                    let t = Instant::now();
                    self.calls = 1;
                    self.last_command = command.clone();
                    let o = self.run_step(step, Some(error));
                    let mut sr = StepResult::new(step, o.status);
                    sr.note = o.note;
                    sr.data = o.data;
                    sr.command = self.last_command.clone();
                    sr.ms = times.get(&failed).copied().unwrap_or(0.0) + t.elapsed().as_secs_f64() * 1000.0;
                    sr.calls = self.calls;
                    sr.batched = true;
                    out.push((*i, sr, o.lost));
                    return (out, *i + 1, None);
                }
                // Refused before it got to the failing step (or it went back): send the rest of
                // these one at a time.
                let from = group.get(completed).map_or(group[0].0, |g| g.0);
                let until = group.get(failed).map_or(from + 1, |g| g.0 + 1);
                (out, from, Some(until))
            }
            Err(e) => {
                let (i, command, _) = &group[0];
                let o = Outcome::lost(&e, &format!("running a batch of {} steps: {}", group.len(), labels()));
                let mut sr = StepResult::new(&design.steps[*i], o.status);
                sr.command = command.clone();
                sr.ms = total_ms;
                sr.calls = 1;
                sr.batched = true;
                out.push((*i, sr, o.lost));
                (out, group[0].0 + 1, None)
            }
        }
    }

    /// Run the designs in order, restarting the session after one is lost.
    pub fn run_all(&mut self, designs: &[&DesignSteps]) -> Vec<DesignResult> {
        designs.iter().map(|d| self.run(d)).collect()
    }

    pub fn run(&mut self, design: &DesignSteps) -> DesignResult {
        let t0 = Instant::now();
        if let Some(why) = self.session_lost.take() {
            match self.transport.restart() {
                Ok(()) => self.event(format!("SolidCraft was started again before {} (the session was given up: {why})", design.name)),
                Err(e) => self.dead = Some(format!("SolidCraft could not be started again after the session was given up ({why}): {e}")),
            }
        }
        if let Some(o) = self.observer.as_deref_mut() {
            o.design_started(design);
        }
        // Ids from another design (or another session) mean nothing here, unless this design
        // works on the one before it.
        if !design.continues {
            self.vars.clear();
        }
        for (k, v) in &self.preset {
            self.vars.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if let Some(d) = &self.steps_dir {
            self.vars.insert(STEPS_DIR.to_string(), Json::from(d.as_str()));
        }
        let mut out = DesignResult { name: design.name.clone(), file: design.file.clone(), saved: false, steps: Vec::new(), ms: 0.0, stopped: None, session_lost: None, vars: BTreeMap::new() };
        if let Some(dead) = self.dead.clone() {
            for (i, step) in design.steps.iter().enumerate() {
                self.finish(design, &mut out, i, StepResult::new(step, Status::NotRun(dead.clone())), None);
            }
            out.ms = t0.elapsed().as_secs_f64() * 1000.0;
            out.vars = self.vars.clone();
            if let Some(o) = self.observer.as_deref_mut() {
                o.design_finished(&out);
            }
            return out;
        }
        let mut i = 0;
        let mut alone_until = 0;
        while i < design.steps.len() {
            let step = &design.steps[i];
            if let Some(lost) = &out.session_lost {
                let why = format!("the SolidCraft session was given up at an earlier step ({}), so this design was not built or saved", clip(lost, 200));
                self.finish(design, &mut out, i, StepResult::new(step, Status::NotRun(why)), None);
                i += 1;
                continue;
            }
            if let Some(why) = out.stopped.clone() {
                // Always try to save what was built.
                if step.tool != "save" {
                    self.finish(design, &mut out, i, StepResult::new(step, Status::NotRun(why)), None);
                    i += 1;
                    continue;
                }
            }
            if self.batching() && i >= alone_until && out.stopped.is_none() {
                let group = self.batch_group(design, i);
                if group.len() >= 2 {
                    let (done, next, alone) = self.run_batch(design, &group);
                    for (k, sr, lost) in done {
                        self.finish(design, &mut out, k, sr, lost);
                    }
                    if let Some(u) = alone {
                        alone_until = u;
                    }
                    i = next;
                    continue;
                }
            }
            let t = Instant::now();
            self.calls = 0;
            self.last_command = step.op.as_deref().and_then(|op| self.profile.ops.get(op)).map(|o| o.command.clone()).unwrap_or_else(|| step.tool.clone());
            let o = self.run_step(step, None);
            let mut sr = StepResult::new(step, o.status);
            sr.note = o.note;
            sr.data = o.data;
            sr.ms = t.elapsed().as_secs_f64() * 1000.0;
            sr.calls = self.calls;
            sr.command = self.last_command.clone();
            self.finish(design, &mut out, i, sr, o.lost);
            i += 1;
        }
        out.ms = t0.elapsed().as_secs_f64() * 1000.0;
        out.vars = self.vars.clone();
        if let Some(o) = self.observer.as_deref_mut() {
            o.design_finished(&out);
        }
        out
    }

    /// Record a step's result: stop the design after a hard failure, note a lost session, and
    /// tell the observer.
    fn finish(&mut self, design: &DesignSteps, out: &mut DesignResult, index: usize, sr: StepResult, lost: Option<String>) {
        let step = &design.steps[index];
        if step.tool == "save" && sr.status == Status::Done {
            out.saved = true;
        }
        if let Some(l) = lost {
            out.session_lost = Some(l.clone());
            self.session_lost = Some(l);
        } else if matches!(sr.status, Status::Failed(_)) && step.on_error == OnError::Stop && out.stopped.is_none() {
            out.stopped = Some(format!("not run: step {} ({}) failed, and the rest of this design needs it", index + 1, step.label));
        }
        if let Some(o) = self.observer.as_deref_mut() {
            o.step_finished(design, index, &sr);
        } else if self.verbose {
            eprintln!("  [{}/{}] {} ({:.1} s)", index + 1, design.steps.len(), sr.label, sr.ms / 1000.0);
            if !sr.status.detail().is_empty() {
                eprintln!("      {}: {}", sr.status.name(), sr.status.detail());
            }
        }
        out.steps.push(sr);
    }
}

/// How one way of a limits search ended.
enum End {
    /// At a contact: the last value clear of it, the two parts, where they touch.
    Contact { at: f64, a: String, b: String, point: Vec<f64> },
    /// It got all the way (a turn, or the reach) without meeting anything.
    Clear,
    /// Its time (or work) ran out first: the last value checked clear.
    Budget { at: Option<f64>, ran_out: String },
    /// A closed loop or another joint's limit stopped it.
    Other { at: Option<f64>, why: String },
    Error(String),
}

impl End {
    fn json(&self) -> Json {
        match self {
            End::Contact { at, a, b, point } => obj! {"stop" => "contact", "at" => *at, "a" => a.as_str(), "b" => b.as_str(), "point" => Json::Arr(point.iter().map(|x| Json::from(*x)).collect())},
            End::Clear => obj! {"stop" => "none"},
            End::Budget { at, ran_out } => obj! {"stop" => "not found", "ran_out" => ran_out.as_str(), "clear_to" => at.map(Json::from).unwrap_or(Json::Null)},
            End::Other { at, why } => obj! {"stop" => "other", "why" => why.as_str(), "at" => at.map(Json::from).unwrap_or(Json::Null)},
            End::Error(e) => obj! {"stop" => "error", "error" => e.as_str()},
        }
    }
}

fn fmt3(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The last line about `joint` in a limits record (JSON lines), if any.
fn earlier_result(path: &str, joint: &str) -> Option<Json> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().filter_map(|l| Json::parse(l).ok()).filter(|j| j.str_of("joint") == Some(joint)).last()
}

fn append_line(path: &str, j: &Json) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", j.to_compact())?;
    f.flush()
}

/// At most `n` characters of `s`.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// The first "(x, y, z)" in a message.
fn first_point(msg: &str) -> Option<[f64; 3]> {
    let mut rest = msg;
    while let Some(i) = rest.find('(') {
        let tail = &rest[i + 1..];
        if let Some(j) = tail.find(')') {
            let v: Vec<f64> = tail[..j].split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if v.len() == 3 {
                return Some([v[0], v[1], v[2]]);
            }
        }
        rest = tail;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::Recorder;
    use crate::steps::var;

    #[test]
    fn binds_ids_and_uses_alternatives() {
        let profile = Profile::builtin();
        let mut rec = Recorder::new();
        rec.fail.push("assembly.constraint.create".into());
        let design = DesignSteps {
            name: "t".into(),
            file: "t.sc".into(),
            steps: vec![
                Step::tool("new_design", Json::obj(), "new"),
                Step::op("insert_design", obj! {"path" => "p.sc", "name" => "p"}, "insert").bind("occ:0", &["/occurrence"]),
                Step::op("place", obj! {"occurrence" => var("occ:0"), "to_matrix" => vec![1.0; 16]}, "place"),
                Step::op("constraint", obj! {"type" => "mate"}, "mate").soft().or_else("joint", obj! {"type" => "planar", "a" => obj!{"occurrence" => var("occ:0")}}),
                Step::op("ground", obj! {"occurrence" => var("missing")}, "ground").soft(),
                Step::tool("save", obj! {"path" => "t.sc"}, "save"),
            ],
            notes: Vec::new(),
            continues: false,
        };
        let mut runner = Runner::new(&mut rec, &profile);
        let r = runner.run(&design);
        assert!(r.saved);
        assert_eq!(r.steps[3].status, Status::Fallback("joint".into()));
        assert!(matches!(r.steps[4].status, Status::Skipped(_)));
        // The placement got the bound occurrence id.
        let place = &rec.calls.iter().find(|(_, a)| a.str_of("command") == Some("occurrence.move")).unwrap().1;
        assert_eq!(place.pointer("params/occurrence").and_then(Json::as_f64), Some(1102.0));
    }

    #[test]
    fn a_conversion_that_loses_volume_is_undone() {
        use crate::mcp::Action;
        let profile = Profile::builtin();
        let vol = |v: f64| Action::Answer(obj! {"total" => obj!{"volume_mm3" => v}});
        // Before, after (half of it gone), after the undo.
        let mut rec = Recorder::new().on("inspect.measure", vol(1000.0), 1).on("inspect.measure", vol(510.0), 1).on("inspect.measure", vol(1000.0), 1);
        let design = DesignSteps {
            name: "t".into(),
            file: "t.sc".into(),
            steps: vec![Step::op("sheet_convert", obj! {"body" => "B", "face" => vec![0.0, 0.0, 0.0]}, "make t sheet metal").soft().keep_volume(), Step::tool("save", obj! {"path" => "t.sc"}, "save")],
            notes: Vec::new(),
            continues: false,
        };
        let r = Runner::new(&mut rec, &profile).run(&design);
        assert!(r.saved);
        match &r.steps[0].status {
            Status::Failed(why) => assert!(why.contains("49.0%") && why.contains("undone"), "{why}"),
            s => panic!("{s:?}"),
        }
        assert!(rec.commands().iter().any(|c| c == "edit.undo"));

        // Within the tolerance it stands.
        let mut rec = Recorder::new().on("inspect.measure", vol(1000.0), 1).on("inspect.measure", vol(1004.0), 1);
        let r = Runner::new(&mut rec, &profile).run(&design);
        assert_eq!(r.steps[0].status, Status::Done);
        assert!(!rec.commands().iter().any(|c| c == "edit.undo"));
    }
}
