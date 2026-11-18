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
}

impl Step {
    pub fn tool(tool: &str, args: Json, label: impl Into<String>) -> Step {
        Step { tool: tool.into(), op: None, args, bind: Vec::new(), on_error: OnError::Stop, label: label.into(), needs: Vec::new(), alternatives: Vec::new() }
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
        j
    }
}

/// A reference to a value bound by an earlier step.
pub fn var(name: &str) -> Json {
    Json::Str(format!("${{{name}}}"))
}

/// Replace every `"${name}"` string in `v` by the bound value (a missing name stays as it is and
/// is reported by the caller).
pub fn substitute(v: &Json, lookup: &dyn Fn(&str) -> Option<Json>, missing: &mut Vec<String>) -> Json {
    match v {
        Json::Str(s) if s.starts_with("${") && s.ends_with('}') => {
            let name = &s[2..s.len() - 1];
            match lookup(name) {
                Some(x) => x,
                None => {
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
}
