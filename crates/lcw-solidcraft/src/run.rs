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
}

impl StepResult {
    fn new(step: &Step, status: Status) -> StepResult {
        StepResult { label: step.label.clone(), group: step.group_name().to_string(), op: step.op.clone().unwrap_or_else(|| step.tool.clone()), status, command: String::new(), ms: 0.0, calls: 0, batched: false, note: String::new() }
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
    /// The session was given up during it.
    lost: Option<String>,
}

impl Outcome {
    fn of(status: Status) -> Outcome {
        Outcome { status, note: String::new(), lost: None }
    }
    fn lost(e: &CallError, what: &str) -> Outcome {
        let msg = e.message().to_string();
        Outcome {
            status: Status::Failed(format!("{msg} (while {what}); the SolidCraft session was given up, since the call may still be running there")),
            note: String::new(),
            lost: Some(msg),
        }
    }
}

pub struct Runner<'a> {
    pub transport: &'a mut dyn Transport,
    pub profile: &'a Profile,
    pub vars: BTreeMap<String, Json>,
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
        self.bind(step, &result);
        Outcome { status, note, lost: None }
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
                Json::Str(s) if s == "budget" => (true, "the search ran out of time before finding the stop; the value is the last one checked clear".to_string()),
                Json::Str(s) => (true, format!("stopped by {s}")),
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
            (value, stops, text)
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
            Ok(_) => {
                let guesses: Vec<&str> = [(&min_text, "min"), (&max_text, "max")].iter().filter(|(t, _)| t.contains("ran out of time")).map(|(_, k)| *k).collect();
                let mut o = Outcome::of(Status::Done);
                if !guesses.is_empty() {
                    o.note = format!("{} end: {}", guesses.join(" and "), if guesses.len() == 1 { "the search ran out of time, so it is the last value checked clear" } else { "the search ran out of time, so they are the last values checked clear" });
                }
                o
            }
            Err(e) if e.ends_session() => Outcome::lost(&e, &format!("setting the limits of {jname}")),
            Err(e) => Outcome::of(Status::Failed(e.message().to_string())),
        }
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
        // Ids from another design (or another session) mean nothing here.
        self.vars.clear();
        if let Some(d) = &self.steps_dir {
            self.vars.insert(STEPS_DIR.to_string(), Json::from(d.as_str()));
        }
        let mut out = DesignResult { name: design.name.clone(), file: design.file.clone(), saved: false, steps: Vec::new(), ms: 0.0, stopped: None, session_lost: None };
        if let Some(dead) = self.dead.clone() {
            for (i, step) in design.steps.iter().enumerate() {
                self.finish(design, &mut out, i, StepResult::new(step, Status::NotRun(dead.clone())), None);
            }
            out.ms = t0.elapsed().as_secs_f64() * 1000.0;
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
            sr.ms = t.elapsed().as_secs_f64() * 1000.0;
            sr.calls = self.calls;
            sr.command = self.last_command.clone();
            self.finish(design, &mut out, i, sr, o.lost);
            i += 1;
        }
        out.ms = t0.elapsed().as_secs_f64() * 1000.0;
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
}
