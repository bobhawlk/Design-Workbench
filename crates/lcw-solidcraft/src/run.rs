//! Run build steps through a SolidCraft session, binding the ids SolidCraft hands back.

use std::collections::BTreeMap;

use lcw_json::{obj, Json};

use crate::mcp::Transport;
use crate::profile::{Catalog, Profile};
use crate::steps::{substitute, DesignSteps, OnError, Step};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Done,
    /// Done with an alternative op (named).
    Fallback(String),
    Skipped(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct StepResult {
    pub label: String,
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct DesignResult {
    pub name: String,
    pub file: String,
    pub saved: bool,
    pub steps: Vec<StepResult>,
}

impl DesignResult {
    pub fn failures(&self) -> impl Iterator<Item = &StepResult> {
        self.steps.iter().filter(|s| matches!(s.status, Status::Failed(_)))
    }
}

pub struct Runner<'a> {
    pub transport: &'a mut dyn Transport,
    pub profile: &'a Profile,
    pub vars: BTreeMap<String, Json>,
    /// Print each step as it runs.
    pub verbose: bool,
}

impl<'a> Runner<'a> {
    pub fn new(transport: &'a mut dyn Transport, profile: &'a Profile) -> Runner<'a> {
        Runner { transport, profile, vars: BTreeMap::new(), verbose: false }
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

    fn execute_op(&mut self, op: &str, args: &Json) -> Result<(Json, Option<String>), String> {
        let resolved = self.profile.resolve(op).ok_or_else(|| format!("no usable SolidCraft command for `{op}`"))?.to_string();
        if resolved != op {
            // A profile fallback for a whole op cannot take the same parameters; only the step's
            // own alternatives can. Refuse instead of guessing.
            return Err(format!("`{op}` is not available in this SolidCraft"));
        }
        let (command, params) = self.profile.call(op, args.clone()).ok_or_else(|| format!("unknown op `{op}`"))?;
        let r = self.transport.call("execute", obj! {"command" => command.as_str(), "params" => params, "verbose" => false})?;
        Ok((r, None))
    }

    fn run_step(&mut self, step: &Step) -> Status {
        for n in &step.needs {
            if !self.vars.contains_key(n) {
                return Status::Skipped(format!("needs `{n}`, which an earlier step did not give"));
            }
        }
        let mut missing = Vec::new();
        let args = substitute(&step.args, &|n| self.lookup(n), &mut missing);
        if !missing.is_empty() {
            return Status::Skipped(format!("needs {}", missing.join(", ")));
        }
        if step.tool == "auto_limits" {
            return self.auto_limits(&args);
        }
        let first = match &step.op {
            Some(op) => self.execute_op(op, &args).map(|(r, _)| r),
            None => self.transport.call(&step.tool, args.clone()),
        };
        let (result, status) = match first {
            Ok(r) => (r, Status::Done),
            Err(e) => {
                let mut outcome = Err(e);
                for (alt, alt_args) in &step.alternatives {
                    let mut miss = Vec::new();
                    let a = substitute(alt_args, &|n| self.lookup(n), &mut miss);
                    if !miss.is_empty() {
                        continue;
                    }
                    if let Ok((r, _)) = self.execute_op(alt, &a) {
                        outcome = Ok((r, alt.clone()));
                        break;
                    }
                }
                match outcome {
                    Ok((r, alt)) => (r, Status::Fallback(alt)),
                    Err(e) => return Status::Failed(e),
                }
            }
        };
        for (name, pointers) in &step.bind {
            if let Some(v) = pointers.iter().find_map(|p| result.pointer(p).filter(|v| !v.is_null()).cloned()) {
                self.vars.insert(name.clone(), v);
            }
        }
        status
    }

    /// Set a joint's limits from `joint.find_limits`, unless nothing stops it (a full turn).
    fn auto_limits(&mut self, args: &Json) -> Status {
        let found = args.get("found").cloned().unwrap_or_default();
        let joint = args.get("joint").cloned().unwrap_or_default();
        let stops = |k: &str| match found.get(k) {
            Some(Json::Str(s)) => !matches!(s.as_str(), "turn" | "range" | "budget"),
            Some(Json::Obj(_)) | Some(Json::Arr(_)) => true,
            _ => false,
        };
        let (Some(min), Some(max)) = (found.f64_of("min"), found.f64_of("max")) else {
            return Status::Skipped("nothing stops it".into());
        };
        if !(stops("min_by") || stops("max_by")) || max - min < 1e-6 {
            return Status::Skipped("it turns freely".into());
        }
        match self.execute_op("set_limits", &obj! {"joint" => joint, "index" => 0, "min" => min, "max" => max}) {
            Ok(_) => Status::Done,
            Err(e) => Status::Failed(e),
        }
    }

    pub fn run(&mut self, design: &DesignSteps) -> DesignResult {
        let mut out = DesignResult { name: design.name.clone(), file: design.file.clone(), saved: false, steps: Vec::new() };
        let mut stopped = false;
        for (i, step) in design.steps.iter().enumerate() {
            if stopped {
                // Always try to save what was built.
                if step.tool != "save" {
                    out.steps.push(StepResult { label: step.label.clone(), status: Status::Skipped("an earlier step failed".into()) });
                    continue;
                }
            }
            if self.verbose {
                eprintln!("  [{}/{}] {}", i + 1, design.steps.len(), step.label);
            }
            let status = self.run_step(step);
            if step.tool == "save" && status == Status::Done {
                out.saved = true;
            }
            if matches!(status, Status::Failed(_)) && step.on_error == OnError::Stop {
                stopped = true;
            }
            if self.verbose {
                if let Status::Failed(e) | Status::Skipped(e) = &status {
                    eprintln!("      {e}");
                }
            }
            out.steps.push(StepResult { label: step.label.clone(), status });
        }
        out
    }
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
