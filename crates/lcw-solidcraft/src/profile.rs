//! The profile: which SolidCraft command and parameter names each of inv2sc's operations uses.
//!
//! SolidCraft changes quickly, so no command name is written into the code that builds the
//! steps. The code asks the profile (`profiles/solidcraft.json`, built in and replaceable with
//! `--profile`), and `inv2sc probe` checks the profile against the running SolidCraft's own
//! command catalogue and schemas before anything is built.

use std::collections::BTreeMap;

use lcw_json::Json;

pub const BUILTIN: &str = include_str!("../../../profiles/solidcraft.json");
pub const SCHEMA: &str = "lcw-solidcraft-profile/1";

#[derive(Debug, Clone)]
pub struct Op {
    pub command: String,
    /// inv2sc's parameter name → SolidCraft's.
    pub params: BTreeMap<String, String>,
    /// Retry after an error that names a point: drop the nearest point from a list parameter
    /// (inv2sc's name) and run again, when the error contains `when`. For example a bend that
    /// closes a loop of bends: leaving it out cuts the sheet open there.
    pub drop_point: Option<DropPoint>,
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
}

impl Profile {
    pub fn builtin() -> Profile {
        // The built-in profile is checked by a test; a broken one cannot ship.
        Profile::parse(BUILTIN).unwrap_or_else(|_| Profile { ops: BTreeMap::new(), fallbacks: BTreeMap::new(), tested_with: Vec::new(), disabled: BTreeMap::new() })
    }

    pub fn parse(text: &str) -> Result<Profile, String> {
        let j = Json::parse(text).map_err(|e| e.to_string())?;
        if j.str_of("schema") != Some(SCHEMA) {
            return Err(format!("not an inv2sc profile (schema should be {SCHEMA})"));
        }
        let mut ops = BTreeMap::new();
        for (name, o) in j.get("ops").and_then(Json::as_object).unwrap_or(&[]) {
            let command = o.str_of("command").ok_or_else(|| format!("op `{name}` has no command"))?.to_string();
            let params = o
                .get("params")
                .and_then(Json::as_object)
                .map(|ps| ps.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
                .unwrap_or_default();
            let drop_point = o.get("retry_dropping_point").and_then(|r| {
                Some(DropPoint { param: r.str_of("param")?.to_string(), when: r.arr_of("when").iter().filter_map(|x| x.as_str().map(str::to_string)).collect() })
            });
            ops.insert(name.clone(), Op { command, params, drop_point });
        }
        let fallbacks = j
            .get("fallbacks")
            .and_then(Json::as_object)
            .map(|f| f.iter().map(|(k, v)| (k.clone(), v.as_array().unwrap_or(&[]).iter().filter_map(|x| x.as_str().map(str::to_string)).collect())).collect())
            .unwrap_or_default();
        let tested_with = j.arr_of("tested_with").iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
        Ok(Profile { ops, fallbacks, tested_with, disabled: BTreeMap::new() })
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
    fn a_missing_command_disables_its_op_and_picks_the_fallback() {
        let mut p = Profile::builtin();
        let mut cat = Catalog::default();
        for op in p.ops.values() {
            cat.commands.insert(op.command.clone(), CommandInfo { enabled: true, ..Default::default() });
        }
        cat.commands.remove("assembly.constraint.create");
        let report = p.check(&cat);
        assert!(report.iter().any(|r| r.contains("constraint") && r.contains("instead")), "{report:?}");
        assert_eq!(p.resolve("constraint"), Some("joint"));
        assert_eq!(p.resolve("place"), Some("place"));
    }
}
