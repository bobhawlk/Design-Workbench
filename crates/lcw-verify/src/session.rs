//! A SolidCraft session for the checker: calls go through the profile (no command name is
//! written here), a design opened stays the current one, and a session that was lost is
//! restarted once and the design opened again.

use std::time::Duration;

use lcw_json::{obj, Json};
use lcw_solidcraft::mcp::{CallError, Transport};
use lcw_solidcraft::profile::Profile;

pub struct Session<'a> {
    pub t: &'a mut dyn Transport,
    pub p: &'a Profile,
    /// The least time a call may take.
    pub timeout: Duration,
    current: Option<String>,
    pub calls: usize,
    pub restarts: usize,
}

impl<'a> Session<'a> {
    pub fn new(t: &'a mut dyn Transport, p: &'a Profile, timeout: Duration) -> Session<'a> {
        Session { t, p, timeout, current: None, calls: 0, restarts: 0 }
    }

    fn send(&mut self, op: &str, params: &Json, timeout: Option<Duration>) -> Result<Json, CallError> {
        if !self.p.available(op) {
            return Err(CallError::Failed(format!("the profile has no usable `{op}` in this SolidCraft")));
        }
        let (command, args) = self.p.call(op, params.clone()).ok_or_else(|| CallError::Failed(format!("unknown op `{op}`")))?;
        let floor = timeout.map_or(self.timeout, |t| t.max(self.timeout));
        let t = self.p.timeout_for(Some(op), "execute", Some(floor));
        self.calls += 1;
        match self.p.tool_of(op) {
            Some(tool) => self.t.call_within(tool, args, t),
            None => self.t.call_within("execute", obj! {"command" => command.as_str(), "params" => args, "verbose" => false}, t),
        }
    }

    /// Run an op. A lost session is restarted once, the current design opened again, and the
    /// call repeated.
    pub fn op(&mut self, op: &str, params: Json) -> Result<Json, String> {
        self.op_within(op, params, None)
    }

    pub fn op_within(&mut self, op: &str, params: Json, timeout: Option<Duration>) -> Result<Json, String> {
        match self.send(op, &params, timeout) {
            Ok(v) => Ok(v),
            Err(e) if e.ends_session() => {
                let why = e.message().to_string();
                self.t.restart().map_err(|r| format!("{why}; and SolidCraft could not be started again: {r}"))?;
                self.restarts += 1;
                if let Some(path) = self.current.clone() {
                    self.send("verify_open", &obj! {"path" => path.as_str()}, None).map_err(|r| format!("{why}; reopening {path}: {}", r.message()))?;
                }
                self.send(op, &params, timeout).map_err(|r| r.message().to_string())
            }
            Err(e) => Err(e.message().to_string()),
        }
    }

    /// Open a design (`.sc` or `.stp`): it becomes the current one.
    pub fn open(&mut self, path: &str) -> Result<Json, String> {
        self.current = None;
        let r = self.op("verify_open", obj! {"path" => path})?;
        self.current = Some(path.to_string());
        Ok(r)
    }
}
