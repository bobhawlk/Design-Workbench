//! Build steps: what to run, in order, and how a step's result feeds later steps.
//!
//! A step is an MCP tool call (`execute` with a command, or a session tool such as `new_design`
//! and `save`). A step may *bind* a value of its result to a name (`occ:1.4` = the occurrence a
//! placement made), and later steps use it as `"${occ:1.4}"`: SolidCraft decides ids, so they
//! are only known while building. The steps are plain JSON (`build-steps.json`), so the same
//! list can be replayed by hand, by another client, or by a SolidCraft plugin.

use lcw_json::{obj, Json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Stop this design (the rest of its steps are not run).
    Stop,
    /// Note it and go on.
    Continue,
}

#[derive(Debug, Clone)]
pub struct Step {
    /// The MCP tool: "execute", "new_design", "save", "open"…
    pub tool: String,
    /// inv2sc's op name for execute steps (resolved through the profile when run).
    pub op: Option<String>,
    /// Tool arguments, or the op's parameters (inv2sc names).
    pub args: Json,
    /// (name, JSON pointers into the result tried in order).
    pub bind: Vec<(String, Vec<String>)>,
    pub on_error: OnError,
    /// What the step is for, in the report.
    pub label: String,
    /// Run only when this name is bound (a step depending on an earlier optional one).
    pub needs: Vec<String>,
    /// Ops to try, in order, when this one fails (each with its own parameters).
    pub alternatives: Vec<(String, Json)>,
    /// What kind of step it is, for the report ("import", "sheet metal", "joint"…); empty: the
    /// op or tool name.
    pub group: String,
    /// Placements that must not move (`{occurrence, path?}`): read before and after the step,
    /// and the step undone and reported when one of them moved. Placement is the truth; a
    /// constraint or joint is kept only when the parts are still where Inventor had them.
    pub guard: Vec<Json>,
}

impl Step {
    pub fn tool(tool: &str, args: Json, label: impl Into<String>) -> Step {
        Step { tool: tool.into(), op: None, args, bind: Vec::new(), on_error: OnError::Stop, label: label.into(), needs: Vec::new(), alternatives: Vec::new(), group: String::new(), guard: Vec::new() }
    }
    pub fn op(op: &str, params: Json, label: impl Into<String>) -> Step {
        Step {
            tool: "execute".into(),
            op: Some(op.into()),
            args: params,
            bind: Vec::new(),
            on_error: OnError::Stop,
            label: label.into(),
            needs: Vec::new(),
            alternatives: Vec::new(),
            group: String::new(),
            guard: Vec::new(),
        }
    }
    /// Undo the step when it moves any of these placements (see [`Step::guard`]).
    pub fn guard(mut self, placements: Vec<Json>) -> Step {
        self.guard = placements;
        self
    }
    pub fn group(mut self, group: &str) -> Step {
        self.group = group.into();
        self
    }
    /// The step's group, or its op or tool when it has none.
    pub fn group_name(&self) -> &str {
        if !self.group.is_empty() {
            &self.group
        } else {
            self.op.as_deref().unwrap_or(&self.tool)
        }
    }
    pub fn bind(mut self, name: impl Into<String>, pointers: &[&str]) -> Step {
        self.bind.push((name.into(), pointers.iter().map(|p| p.to_string()).collect()));
        self
    }
    pub fn soft(mut self) -> Step {
        self.on_error = OnError::Continue;
        self
    }
    pub fn needs(mut self, name: impl Into<String>) -> Step {
        self.needs.push(name.into());
        self
    }
    pub fn or_else(mut self, op: &str, params: Json) -> Step {
        self.alternatives.push((op.into(), params));
        self
    }

    pub fn to_json(&self) -> Json {
        let mut j = obj! {"tool" => &self.tool, "label" => &self.label};
        if let Some(op) = &self.op {
            j.set("op", op.as_str());
        }
        j.set("args", self.args.clone());
        if !self.bind.is_empty() {
            let mut b = Json::obj();
            for (k, ps) in &self.bind {
                b.set(k, ps.clone());
            }
            j.set("bind", b);
        }
        if self.on_error == OnError::Continue {
            j.set("on_error", "continue");
        }
        if !self.needs.is_empty() {
            j.set("needs", self.needs.clone());
        }
        if !self.alternatives.is_empty() {
            j.set("alternatives", Json::Arr(self.alternatives.iter().map(|(o, p)| obj! {"op" => o.as_str(), "args" => p.clone()}).collect()));
        }
        if !self.group.is_empty() {
            j.set("group", self.group.as_str());
        }
        if !self.guard.is_empty() {
            j.set("guard", Json::Arr(self.guard.clone()));
        }
        j
    }

    /// A step as [`Step::to_json`] writes it.
    pub fn from_json(j: &Json) -> Result<Step, String> {
        let tool = j.str_of("tool").ok_or("a step has no `tool`")?.to_string();
        let strings = |v: &Json| -> Vec<String> {
            match v {
                Json::Str(s) => vec![s.clone()],
                Json::Arr(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            }
        };
        Ok(Step {
            op: j.str_of("op").map(str::to_string),
            args: j.get("args").cloned().unwrap_or_else(Json::obj),
            bind: j.get("bind").and_then(Json::as_object).unwrap_or(&[]).iter().map(|(k, v)| (k.clone(), strings(v))).collect(),
            on_error: if j.str_of("on_error") == Some("continue") { OnError::Continue } else { OnError::Stop },
            label: j.str_of("label").unwrap_or(&tool).to_string(),
            needs: j.get("needs").map(strings).unwrap_or_default(),
            alternatives: j.arr_of("alternatives").iter().filter_map(|a| Some((a.str_of("op")?.to_string(), a.get("args").cloned().unwrap_or_else(Json::obj)))).collect(),
            group: j.str_of("group").unwrap_or_default().to_string(),
            guard: j.arr_of("guard").to_vec(),
            tool,
        })
    }
}

/// The name build steps use for the folder the steps file is in (`--relative-paths`): a path
/// written `"${steps_dir}/parts/x.stp"` is resolved when the steps are run.
pub const STEPS_DIR: &str = "steps_dir";

/// A path relative to the steps file, resolved at run time.
pub fn relative_path(rest: &str) -> String {
    format!("${{{STEPS_DIR}}}/{}", rest.replace('\\', "/").trim_start_matches('/'))
}

/// `"${name}"` (the whole value), or `"${name}/rest"` (a path under a bound folder).
fn reference(s: &str) -> Option<(&str, Option<&str>)> {
    let body = s.strip_prefix("${")?;
    let end = body.find('}')?;
    let (name, rest) = (&body[..end], &body[end + 1..]);
    if rest.is_empty() {
        Some((name, None))
    } else if rest.starts_with('/') || rest.starts_with('\\') {
        Some((name, Some(rest)))
    } else {
        None
    }
}

/// The names `v` refers to.
pub fn references(v: &Json, out: &mut Vec<String>) {
    match v {
        Json::Str(s) => {
            if let Some((name, _)) = reference(s) {
                out.push(name.to_string());
            }
        }
        Json::Arr(items) => items.iter().for_each(|x| references(x, out)),
        Json::Obj(items) => items.iter().for_each(|(_, x)| references(x, out)),
        _ => {}
    }
}

/// A reference to a value bound by an earlier step.
pub fn var(name: &str) -> Json {
    Json::Str(format!("${{{name}}}"))
}

/// Replace every `"${name}"` string in `v` by the bound value (a missing name stays as it is and
/// is reported by the caller).
///
/// `"${name}/rest"` is a path under the folder `name` is bound to, written with this machine's
/// separators.
pub fn substitute(v: &Json, lookup: &dyn Fn(&str) -> Option<Json>, missing: &mut Vec<String>) -> Json {
    match v {
        Json::Str(s) if reference(s).is_some() => {
            let (name, rest) = reference(s).unwrap_or_default();
            match (lookup(name), rest) {
                (Some(x), None) => x,
                (Some(Json::Str(base)), Some(rest)) => {
                    let sep = std::path::MAIN_SEPARATOR;
                    let rest: String = rest.chars().map(|c| if c == '/' || c == '\\' { sep } else { c }).collect();
                    Json::Str(format!("{}{rest}", base.trim_end_matches(['/', '\\'])))
                }
                _ => {
                    missing.push(name.to_string());
                    v.clone()
                }
            }
        }
        Json::Arr(items) => Json::Arr(items.iter().map(|x| substitute(x, lookup, missing)).collect()),
        Json::Obj(items) => Json::Obj(items.iter().map(|(k, x)| (k.clone(), substitute(x, lookup, missing))).collect()),
        other => other.clone(),
    }
}

/// One design to build: a part or the assembly.
#[derive(Debug, Clone)]
pub struct DesignSteps {
    pub name: String,
    /// The .sc file it is saved as.
    pub file: String,
    pub steps: Vec<Step>,
    /// What was left out of this design and why, for the report.
    pub notes: Vec<String>,
}

impl DesignSteps {
    pub fn from_json(j: &Json) -> Result<DesignSteps, String> {
        let steps = j.arr_of("steps").iter().map(Step::from_json).collect::<Result<Vec<_>, _>>()?;
        Ok(DesignSteps { name: j.str_of("name").unwrap_or("design").to_string(), file: j.str_of("file").unwrap_or_default().to_string(), steps, notes: Vec::new() })
    }
}

/// The designs of a `build-steps.json` (`lcw-build-steps/1`), in build order: parts, then the
/// assembly.
pub fn designs_from_json(j: &Json) -> Result<Vec<DesignSteps>, String> {
    if j.str_of("schema") != Some("lcw-build-steps/1") {
        return Err("not an inv2sc build-steps file (schema should be lcw-build-steps/1)".into());
    }
    let mut out = j.arr_of("parts").iter().map(DesignSteps::from_json).collect::<Result<Vec<_>, _>>()?;
    if let Some(a) = j.get("assembly").filter(|a| !a.is_null()) {
        out.push(DesignSteps::from_json(a)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_bound_values_anywhere() {
        let v = obj! {"occurrence" => var("occ:1"), "list" => vec![var("occ:1"), var("nope")], "keep" => "x${occ:1}"};
        let mut missing = Vec::new();
        let out = substitute(&v, &|n| (n == "occ:1").then(|| Json::from(7)), &mut missing);
        assert_eq!(out.to_compact(), r#"{"occurrence":7,"list":[7,"${nope}"],"keep":"x${occ:1}"}"#);
        assert_eq!(missing, vec!["nope".to_string()]);
    }

    #[test]
    fn paths_under_the_steps_folder_resolve_at_run_time() {
        let sep = std::path::MAIN_SEPARATOR;
        let v = obj! {"path" => relative_path("parts\\Bracket.stp"), "other" => "${steps_dir}x"};
        assert_eq!(v.str_of("path"), Some("${steps_dir}/parts/Bracket.stp"));
        let mut missing = Vec::new();
        let out = substitute(&v, &|n| (n == STEPS_DIR).then(|| Json::from("/work/out/")), &mut missing);
        assert_eq!(out.str_of("path"), Some(format!("/work/out{sep}parts{sep}Bracket.stp").as_str()));
        assert_eq!(out.str_of("other"), Some("${steps_dir}x"), "not a path reference");
        let mut none = Vec::new();
        let _ = substitute(&v, &|_| None, &mut none);
        assert_eq!(none, vec![STEPS_DIR.to_string()]);
        let mut refs = Vec::new();
        references(&obj! {"a" => var("occ:1"), "b" => vec![relative_path("x.sc")]}, &mut refs);
        assert_eq!(refs, vec!["occ:1".to_string(), STEPS_DIR.to_string()]);
    }

    #[test]
    fn steps_read_back_as_written() {
        let s = Step::op("constraint", obj! {"type" => "mate", "a" => var("occ:1")}, "mate it")
            .soft()
            .needs("occ:1")
            .bind("c", &["/id", "/name"])
            .or_else("joint", obj! {"type" => "planar"})
            .group("constraint");
        let back = Step::from_json(&Json::parse(&s.to_json().to_compact()).unwrap()).unwrap();
        assert_eq!(back.to_json(), s.to_json());
        assert_eq!(back.group_name(), "constraint");
        assert_eq!(Step::tool("save", Json::obj(), "save").group_name(), "save");
    }
}
