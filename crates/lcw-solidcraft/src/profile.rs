//! The profile: which SolidCraft command and parameter names each of inv2sc's operations uses.
//!
//! SolidCraft changes quickly, so no command name is written into the code that builds the
//! steps. The code asks the profile (`profiles/solidcraft.json`, built in and replaceable with
//! `--profile`), and `inv2sc probe` checks the profile against the running SolidCraft's own
//! command catalogue and schemas before anything is built.

use std::collections::BTreeMap;

use std::time::Duration;

use lcw_json::Json;

pub const BUILTIN: &str = include_str!("../../../profiles/solidcraft.json");
pub const SCHEMA: &str = "lcw-solidcraft-profile/1";

#[derive(Debug, Clone)]
pub struct Op {
    /// The SolidCraft command run through `execute` (empty for an op that is a session tool).
    pub command: String,
    /// The MCP session tool this op calls directly (`open`, `measure`…), when it is not a
    /// command. Used by `inv2sc verify`.
    pub tool: Option<String>,
    /// inv2sc's parameter name → SolidCraft's.
    pub params: BTreeMap<String, String>,
    /// Retry after an error that names a point: drop the nearest point from a list parameter
    /// (inv2sc's name) and run again, when the error contains `when`. For example a bend that
    /// closes a loop of bends: leaving it out cuts the sheet open there.
    pub drop_point: Option<DropPoint>,
    /// How long one call of this op may take (`timeout_s`), when it is known to be slow.
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct DropPoint {
    pub param: String,
    pub when: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub ops: BTreeMap<String, Op>,
    pub fallbacks: BTreeMap<String, Vec<String>>,
    pub tested_with: Vec<String>,
    /// Ops a probe found missing or changed (filled by [`Profile::check`]).
    pub disabled: BTreeMap<String, String>,
    /// How the runner calls SolidCraft beyond `execute` (`runner`).
    pub runner: RunnerProfile,
    /// Where `inv2sc verify` finds things in SolidCraft's answers (`verify`).
    /// How the mechanism check (`verify_joints`) reads SolidCraft's answers (`mechanism`):
    /// JSON pointers and keys into the replies of the `joint_list` and `drive` ops, and how far
    /// to drive each joint.
    pub mechanism: Json,
    pub verify: Json,
}

/// The `runner` section: timeouts, the `batch` tool and the answer of a call still running.
#[derive(Debug, Clone)]
pub struct RunnerProfile {
    /// A call's timeout when its op or tool names none.
    pub timeout: Duration,
    /// Session tools' own timeouts (`save`, `open`…).
    pub tool_timeouts: BTreeMap<String, Duration>,
    pub batch: Option<BatchProfile>,
    pub still_running: Option<StillRunning>,
}

/// SolidCraft's `batch` tool: its name, its argument names and where its reply keeps things.
#[derive(Debug, Clone)]
pub struct BatchProfile {
    pub tool: String,
    pub commands: String,
    pub step_command: String,
    pub step_params: String,
    /// Fixed arguments sent with every batch (rollback off, timings on…).
    pub args: Json,
    /// Pointers into the reply (a success, or the error text of a failure parsed as JSON).
    pub results: String,
    pub failed_index: String,
    pub failed_error: String,
    pub rolled_back: String,
    pub timings: String,
    pub timing_index: String,
    pub timing_ms: String,
}

/// A bridged call that outlived the bridge's own wait answers with an error starting `marker`;
/// `pending_tool` waits for its result and `cancel_tool` cancels it.
#[derive(Debug, Clone)]
pub struct StillRunning {
    pub marker: String,
    pub pending_tool: String,
    pub done: String,
    pub result: String,
    pub cancel_tool: Option<String>,
}

/// The timeout of a call when nothing else names one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

impl Default for RunnerProfile {
    fn default() -> RunnerProfile {
        RunnerProfile { timeout: DEFAULT_TIMEOUT, tool_timeouts: BTreeMap::new(), batch: None, still_running: None }
    }
}

fn seconds(j: Option<&Json>) -> Option<Duration> {
    j.and_then(Json::as_f64).filter(|s| s.is_finite() && *s > 0.0).map(Duration::from_secs_f64)
}

impl RunnerProfile {
    fn parse(j: Option<&Json>) -> Result<RunnerProfile, String> {
        let Some(j) = j else { return Ok(RunnerProfile::default()) };
        let mut r = RunnerProfile { timeout: seconds(j.get("timeout_s")).unwrap_or(DEFAULT_TIMEOUT), ..RunnerProfile::default() };
        for (k, v) in j.get("tool_timeout_s").and_then(Json::as_object).unwrap_or(&[]) {
            if let Some(d) = seconds(Some(v)) {
                r.tool_timeouts.insert(k.clone(), d);
            }
        }
        if let Some(b) = j.get("batch").filter(|b| !b.is_null()) {
            let s = |k: &str| b.str_of(k).map(str::to_string).ok_or_else(|| format!("runner.batch has no `{k}`"));
            r.batch = Some(BatchProfile {
                tool: s("tool")?,
                commands: s("commands")?,
                step_command: s("step_command")?,
                step_params: s("step_params")?,
                args: b.get("args").cloned().filter(|a| a.as_object().is_some()).unwrap_or_else(Json::obj),
                results: s("results")?,
                failed_index: s("failed_index")?,
                failed_error: s("failed_error")?,
                rolled_back: s("rolled_back")?,
                timings: b.str_of("timings").unwrap_or_default().to_string(),
                timing_index: b.str_of("timing_index").unwrap_or_default().to_string(),
                timing_ms: b.str_of("timing_ms").unwrap_or_default().to_string(),
            });
        }
        if let Some(w) = j.get("still_running").filter(|w| !w.is_null()) {
            let s = |k: &str| w.str_of(k).map(str::to_string).ok_or_else(|| format!("runner.still_running has no `{k}`"));
            r.still_running = Some(StillRunning { marker: s("marker")?, pending_tool: s("pending_tool")?, done: s("done")?, result: s("result")?, cancel_tool: w.str_of("cancel_tool").map(str::to_string) });
        }
        Ok(r)
    }
}

impl Profile {
    pub fn builtin() -> Profile {
        // The built-in profile is checked by a test; a broken one cannot ship.
        Profile::parse(BUILTIN).unwrap_or_else(|_| Profile { ops: BTreeMap::new(), fallbacks: BTreeMap::new(), tested_with: Vec::new(), disabled: BTreeMap::new(), runner: RunnerProfile::default(), verify: Json::Null, mechanism: Json::Null })
    }

    pub fn parse(text: &str) -> Result<Profile, String> {
        let j = Json::parse(text).map_err(|e| e.to_string())?;
        if j.str_of("schema") != Some(SCHEMA) {
            return Err(format!("not an inv2sc profile (schema should be {SCHEMA})"));
        }
        let mut ops = BTreeMap::new();
        for (name, o) in j.get("ops").and_then(Json::as_object).unwrap_or(&[]) {
            let tool = o.str_of("tool").map(str::to_string);
            let command = match (o.str_of("command"), &tool) {
                (Some(c), _) => c.to_string(),
                (None, Some(_)) => String::new(),
                (None, None) => return Err(format!("op `{name}` has no command or tool")),
            };
            let params = o
                .get("params")
                .and_then(Json::as_object)
                .map(|ps| ps.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
                .unwrap_or_default();
            let drop_point = o.get("retry_dropping_point").and_then(|r| {
                Some(DropPoint { param: r.str_of("param")?.to_string(), when: r.arr_of("when").iter().filter_map(|x| x.as_str().map(str::to_string)).collect() })
            });
            ops.insert(name.clone(), Op { command, tool, params, drop_point, timeout: seconds(o.get("timeout_s")) });
        }
        let fallbacks = j
            .get("fallbacks")
            .and_then(Json::as_object)
            .map(|f| f.iter().map(|(k, v)| (k.clone(), v.as_array().unwrap_or(&[]).iter().filter_map(|x| x.as_str().map(str::to_string)).collect())).collect())
            .unwrap_or_default();
        let tested_with = j.arr_of("tested_with").iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
        let runner = RunnerProfile::parse(j.get("runner"))?;
        let verify = j.get("verify").cloned().unwrap_or(Json::Null);
        let mechanism = j.get("mechanism").cloned().unwrap_or(Json::Null);
        Ok(Profile { ops, fallbacks, tested_with, disabled: BTreeMap::new(), runner, verify, mechanism })
    }

    /// How long one call of `op` (or, without an op, of the session tool `tool`) may take:
    /// the op's or tool's own timeout, never less than `floor` (the run's `--timeout`).
    pub fn timeout_for(&self, op: Option<&str>, tool: &str, floor: Option<Duration>) -> Duration {
        let own = match op {
            Some(op) => self.ops.get(op).and_then(|o| o.timeout),
            None => self.runner.tool_timeouts.get(tool).copied(),
        };
        let base = floor.unwrap_or(self.runner.timeout);
        own.map_or(base, |t| t.max(base))
    }

    /// The name SolidCraft uses for `key` in the `verify` section's `group` (the key itself when
    /// the profile does not rename it).
    pub fn vname(&self, group: &str, key: &str) -> String {
        self.verify.get(group).and_then(|g| g.str_of(key)).unwrap_or(key).to_string()
    }

    /// The session tool an op calls directly instead of a command (`open`, `measure`).
    pub fn tool_of(&self, op: &str) -> Option<&str> {
        self.ops.get(op).and_then(|o| o.tool.as_deref())
    }

    pub fn available(&self, op: &str) -> bool {
        self.ops.contains_key(op) && !self.disabled.contains_key(op)
    }

    /// The command and SolidCraft-named parameters for an op called with inv2sc's names.
    /// Parameters the profile doesn't name pass through unchanged; `null` ones are left out.
    pub fn call(&self, op: &str, params: Json) -> Option<(String, Json)> {
        let o = self.ops.get(op)?;
        let mut out = Json::obj();
        for (k, v) in params.as_object().unwrap_or(&[]) {
            if v.is_null() {
                continue;
            }
            let name = o.params.get(k).cloned().unwrap_or_else(|| k.clone());
            out.set(&name, v.clone());
        }
        Some((o.command.clone(), out))
    }

    /// Check every op against a catalogue (`engine.commands` / MCP `list_commands`) and, when
    /// given, each command's described parameters. Ops that no longer fit are disabled with the
    /// reason, so the build falls back or reports instead of failing half way.
    pub fn check(&mut self, catalog: &Catalog) -> Vec<String> {
        let mut report = Vec::new();
        for (name, op) in &self.ops {
            if op.tool.is_some() {
                continue;
            }
            match catalog.commands.get(&op.command) {
                None => {
                    self.disabled.insert(name.clone(), format!("SolidCraft has no command `{}`", op.command));
                }
                Some(info) if !info.enabled => {
                    report.push(format!("{name}: `{}` is disabled right now ({})", op.command, info.reason.clone().unwrap_or_default()));
                }
                Some(info) => {
                    let missing: Vec<&String> = op.params.values().filter(|p| !info.knows_param(p)).collect();
                    if !missing.is_empty() && info.schema_params.is_some() {
                        report.push(format!(
                            "{name}: `{}` does not list parameter(s) {} any more; check the profile",
                            op.command,
                            missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")
                        ));
                    }
                }
            }
        }
        for (name, why) in &self.disabled {
            let alt = self.fallbacks.get(name).and_then(|f| f.iter().find(|a| self.ops.contains_key(*a) && !self.disabled.contains_key(*a)));
            report.push(match alt {
                Some(a) => format!("{name}: {why}; using `{a}` instead"),
                None => format!("{name}: {why}; this step will be skipped"),
            });
        }
        report
    }

    /// The op to use for `op`: itself, or its first usable fallback.
    pub fn resolve<'a>(&'a self, op: &'a str) -> Option<&'a str> {
        if self.available(op) {
            return Some(op);
        }
        self.fallbacks.get(op)?.iter().find(|a| self.available(a)).map(String::as_str)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandInfo {
    pub enabled: bool,
    pub reason: Option<String>,
    /// The one-line parameter docs.
    pub params_doc: String,
    /// Parameter names from the command's JSON Schema, when it has one.
    pub schema_params: Option<Vec<String>>,
}

impl CommandInfo {
    /// A parameter is known when the schema lists it, or (without a schema) the docs name it.
    pub fn knows_param(&self, p: &str) -> bool {
        match &self.schema_params {
            Some(list) => list.iter().any(|x| x == p),
            None => self.params_doc.contains(p),
        }
    }
}

/// SolidCraft's command catalogue.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub version: Option<String>,
    pub commands: BTreeMap<String, CommandInfo>,
}

impl Catalog {
    /// From `engine.commands` / `list_commands` output: an array, or {commands: [...]}.
    pub fn from_json(j: &Json) -> Catalog {
        let list = j.as_array().or_else(|| j.get("commands").and_then(Json::as_array)).unwrap_or(&[]);
        let mut commands = BTreeMap::new();
        for c in list {
            let Some(id) = c.str_of("id") else { continue };
            commands.insert(
                id.to_string(),
                CommandInfo {
                    enabled: c.bool_of("enabled").unwrap_or(true),
                    reason: c.str_of("disabled_reason").map(str::to_string),
                    params_doc: c.str_of("params").unwrap_or_default().to_string(),
                    schema_params: None,
                },
            );
        }
        Catalog { version: None, commands }
    }

    /// Add a command's input schema (`describe_command` → `input`).
    pub fn add_schema(&mut self, id: &str, describe: &Json) {
        let props = describe.pointer("input/properties").and_then(Json::as_object);
        if let (Some(info), Some(props)) = (self.commands.get_mut(id), props) {
            info.schema_params = Some(props.iter().map(|(k, _)| k.clone()).collect());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcw_json::obj;

    #[test]
    fn the_builtin_profile_parses_and_renames() {
        let p = Profile::parse(BUILTIN).expect("built-in profile");
        assert!(p.ops.len() >= 15);
        let (cmd, params) = p.call("place", obj! {"occurrence" => 3, "to_matrix" => vec![1.0, 0.0], "nothing" => Json::Null}).unwrap();
        assert_eq!(cmd, "occurrence.move");
        assert_eq!(params.to_compact(), r#"{"occurrence":3,"to_matrix":[1,0]}"#);
    }

    #[test]
    fn verify_ops_may_be_session_tools_and_the_verify_section_renames_keys() {
        let mut p = Profile::builtin();
        assert_eq!(p.tool_of("verify_open"), Some("open"));
        assert_eq!(p.tool_of("list_components"), None);
        assert_eq!(p.call("verify_measure", obj! {"summary" => true}).unwrap().1.to_compact(), r#"{"summary":true}"#);
        assert_eq!(p.vname("props", "com"), "center_of_mass");
        assert_eq!(p.vname("props", "unknown_key"), "unknown_key");
        // A tool op is not looked for in the command catalogue.
        let report = p.check(&Catalog { version: None, commands: Default::default() });
        assert!(!report.iter().any(|r| r.starts_with("verify_open") || r.starts_with("verify_measure")), "{report:?}");
    }

    #[test]
    fn a_missing_command_disables_its_op_and_picks_the_fallback() {
        let mut p = Profile::builtin();
        let mut cat = Catalog::default();
        for op in p.ops.values() {
            cat.commands.insert(op.command.clone(), CommandInfo { enabled: true, ..Default::default() });
        }
        cat.commands.remove("joint.rigid_group");
        cat.commands.remove("assembly.constraint.create");
        let report = p.check(&cat);
        assert!(report.iter().any(|r| r.contains("rigid_group") && r.contains("instead")), "{report:?}");
        assert_eq!(p.resolve("rigid_group"), Some("joint"));
        // A constraint has no fallback: a joint guessed from its rule frees or locks parts.
        assert!(report.iter().any(|r| r.starts_with("constraint:") && r.contains("skipped")), "{report:?}");
        assert_eq!(p.resolve("constraint"), None);
        assert_eq!(p.resolve("place"), Some("place"));
    }

    #[test]
    fn timeouts_come_from_the_profile_and_the_floor_raises_them() {
        let p = Profile::builtin();
        assert_eq!(p.timeout_for(Some("place"), "execute", None), Duration::from_secs(300));
        assert_eq!(p.timeout_for(Some("find_limits"), "execute", None), Duration::from_secs(900));
        assert_eq!(p.timeout_for(None, "save", None), Duration::from_secs(600));
        assert_eq!(p.timeout_for(Some("find_limits"), "execute", Some(Duration::from_secs(1200))), Duration::from_secs(1200));
        assert_eq!(p.timeout_for(Some("place"), "execute", Some(Duration::from_secs(5))), Duration::from_secs(5));
        assert!(p.runner.batch.is_some() && p.runner.still_running.is_some());
        // An older profile without a runner section still works, without batching.
        let old = Profile::parse(r#"{"schema": "lcw-solidcraft-profile/1", "ops": {"place": {"command": "x.y"}}}"#).unwrap();
        assert!(old.runner.batch.is_none());
        assert_eq!(old.timeout_for(Some("place"), "execute", None), DEFAULT_TIMEOUT);
    }
}
