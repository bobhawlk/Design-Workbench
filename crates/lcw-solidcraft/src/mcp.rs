//! A Model Context Protocol client for `solidcraft-cli mcp` over stdio.
//!
//! The MCP server is SolidCraft's own, supported interface for programs: the same commands the UI
//! runs, with a catalogue and JSON Schemas. Headless (`solidcraft-cli mcp`) it builds designs in
//! its own session; with `--connect HOST:PORT` it drives the running app, so the user watches the
//! assembly being built.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use lcw_json::{obj, Json};

/// Why a call has no result.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// SolidCraft answered with an error: the session is fine.
    Failed(String),
    /// No answer within the time allowed. The call may still be running in SolidCraft, so the
    /// session can't be trusted any more (the transport has given it up).
    TimedOut(String),
    /// The session is gone (the server stopped, or it was given up after a timeout).
    Lost(String),
}

impl CallError {
    pub fn message(&self) -> &str {
        match self {
            CallError::Failed(m) | CallError::TimedOut(m) | CallError::Lost(m) => m,
        }
    }
    /// The session is no longer usable.
    pub fn ends_session(&self) -> bool {
        !matches!(self, CallError::Failed(_))
    }
}

/// What the runner needs from a SolidCraft session.
pub trait Transport {
    /// Call an MCP tool; Ok is the tool's result (its text content parsed as JSON when it is
    /// JSON), Err the tool's or the protocol's error message.
    fn call(&mut self, tool: &str, args: Json) -> Result<Json, String>;

    /// [`Transport::call`] with a time limit, telling a SolidCraft error from a lost session.
    fn call_within(&mut self, tool: &str, args: Json, timeout: Duration) -> Result<Json, CallError> {
        let _ = timeout;
        self.call(tool, args).map_err(CallError::Failed)
    }

    /// Give up the session and start a fresh one (after a timeout or a lost session). Designs
    /// being built in the old one are lost; saved files are not.
    fn restart(&mut self) -> Result<(), String> {
        Err("this SolidCraft session can't be restarted".into())
    }
}

/// How to start the MCP server (kept to restart it).
#[derive(Debug, Clone)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Launch {
    pub fn new(program: &str, args: &[String]) -> Launch {
        Launch { program: program.into(), args: args.to_vec(), env: Vec::new() }
    }
}

pub struct McpClient {
    launch: Launch,
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// The timeout of calls made without one (`call`, the handshake).
    pub timeout: Duration,
    pub server_info: Json,
    /// Why the session was given up, once it was.
    broken: Option<String>,
}

impl McpClient {
    /// Start `program args…` and initialise the MCP session.
    pub fn spawn(program: &str, args: &[String]) -> Result<McpClient, String> {
        McpClient::start(Launch::new(program, args), crate::profile::DEFAULT_TIMEOUT)
    }

    pub fn start(launch: Launch, timeout: Duration) -> Result<McpClient, String> {
        let (child, stdin, lines) = open(&launch)?;
        let mut c = McpClient { launch, child, stdin, lines, next_id: 1, timeout, server_info: Json::Null, broken: None };
        c.handshake()?;
        Ok(c)
    }

    fn handshake(&mut self) -> Result<(), String> {
        let init = self
            .request(
                "initialize",
                obj! {"protocolVersion" => "2025-06-18", "capabilities" => Json::obj(), "clientInfo" => obj!{"name" => "inv2sc", "version" => env!("CARGO_PKG_VERSION")}},
                self.timeout,
            )
            .map_err(|e| e.message().to_string())?;
        self.server_info = init.get("serverInfo").cloned().unwrap_or_default();
        self.notify("notifications/initialized", Json::obj()).map_err(|e| e.message().to_string())
    }

    fn send(&mut self, msg: &Json) -> Result<(), CallError> {
        let mut line = msg.to_compact();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()).map_err(|e| self.give_up(format!("SolidCraft's MCP server closed its input: {e}")))
    }

    fn notify(&mut self, method: &str, params: Json) -> Result<(), CallError> {
        self.send(&obj! {"jsonrpc" => "2.0", "method" => method, "params" => params})
    }

    /// Stop the server and remember why: a call that timed out may still be running in it, and
    /// its late answer or its changes would land in whatever is sent next.
    fn give_up(&mut self, why: String) -> CallError {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.broken = Some(why.clone());
        CallError::Lost(why)
    }

    pub fn request(&mut self, method: &str, params: Json, timeout: Duration) -> Result<Json, CallError> {
        if let Some(why) = &self.broken {
            return Err(CallError::Lost(format!("the SolidCraft session was given up earlier ({why})")));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send(&obj! {"jsonrpc" => "2.0", "id" => id, "method" => method, "params" => params})?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    let what = format!("no answer from SolidCraft within {} s", timeout.as_secs_f64().round());
                    let _ = self.give_up(what.clone());
                    return Err(CallError::TimedOut(what));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let status = self.child.try_wait().ok().flatten().map(|s| format!(" ({s})")).unwrap_or_default();
                    return Err(self.give_up(format!("SolidCraft's MCP server stopped{status}")));
                }
            };
            let Ok(msg) = Json::parse(&line) else { continue };
            // Server requests and notifications (logging, progress) are not ours.
            if msg.get("id").and_then(Json::as_u64) != Some(id) || msg.get("method").is_some() {
                continue;
            }
            if let Some(e) = msg.get("error") {
                return Err(CallError::Failed(e.str_of("message").unwrap_or("MCP error").to_string()));
            }
            return Ok(msg.get("result").cloned().unwrap_or_default());
        }
    }
}

/// Start the server and a thread reading its output lines.
fn open(launch: &Launch) -> Result<(Child, ChildStdin, Receiver<String>), String> {
    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start `{}`: {e} (is SolidCraft installed? pass --solidcraft-cli PATH)", launch.program))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout: ChildStdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    Ok((child, stdin, rx))
}

/// A `tools/call` result as the tool's value, or its error message.
pub fn decode_tool_result(r: &Json) -> Result<Json, String> {
    let text: String = r.arr_of("content").iter().filter(|c| c.str_of("type") == Some("text")).filter_map(|c| c.str_of("text")).collect::<Vec<_>>().join("\n");
    let value = r.get("structuredContent").cloned().filter(|v| !v.is_null()).or_else(|| Json::parse(&text).ok()).unwrap_or(Json::Str(text.clone()));
    if r.bool_of("isError").unwrap_or(false) {
        let msg = value.str_of("error").map(str::to_string).or_else(|| value.as_str().map(str::to_string)).unwrap_or(text);
        return Err(msg);
    }
    if let Some(e) = value.get("error").and_then(Json::as_str) {
        if value.get("ok").and_then(Json::as_bool) == Some(false) || value.as_object().is_some_and(|o| o.len() <= 2) {
            return Err(e.to_string());
        }
    }
    Ok(value)
}

impl Transport for McpClient {
    fn call(&mut self, tool: &str, args: Json) -> Result<Json, String> {
        let t = self.timeout;
        self.call_within(tool, args, t).map_err(|e| e.message().to_string())
    }

    fn call_within(&mut self, tool: &str, args: Json, timeout: Duration) -> Result<Json, CallError> {
        let r = self.request("tools/call", obj! {"name" => tool, "arguments" => args}, timeout)?;
        decode_tool_result(&r).map_err(CallError::Failed)
    }

    fn restart(&mut self) -> Result<(), String> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let (child, stdin, lines) = open(&self.launch)?;
        self.child = child;
        self.stdin = stdin;
        self.lines = lines;
        self.next_id = 1;
        self.broken = None;
        self.handshake()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a scripted call does instead of the default answer.
#[derive(Debug, Clone)]
pub enum Action {
    /// Answer this.
    Answer(Json),
    /// Fail with this SolidCraft error.
    Fail(String),
    /// Take this long, then answer as usual (or time out first).
    Delay(Duration),
    /// Never answer: the call times out.
    Hang,
    /// The server stops.
    Die,
    /// A bridged call outliving the bridge's wait: an error starting with the profile's
    /// still-running marker, then `pending` answers "not done" this many times before the result.
    StillRunning(usize),
}

/// A scripted answer: for calls of this command (an `execute` or `batch` step's command id) or
/// tool, `times` times (0: every time).
#[derive(Debug, Clone)]
pub struct Rule {
    pub when: String,
    pub action: Action,
    pub times: usize,
}

/// A fake SolidCraft session (tests, dry runs): answers every call with plausible ids, records
/// it, and follows a script that can fail, delay, hang or stop calls. It runs `batch` as
/// SolidCraft does: in order, stopping at the first error, without rolling back.
pub struct Recorder {
    /// Every call: (tool, arguments). A batch's steps are also recorded one by one as
    /// ("batch step", {command, params}).
    pub calls: Vec<(String, Json)>,
    next: u64,
    /// Commands to fail (by id), for testing fallbacks.
    pub fail: Vec<String>,
    pub rules: Vec<Rule>,
    /// Session tools taken as not existing (for example "batch").
    pub missing_tools: Vec<String>,
    pub restarts: usize,
    /// `cancel` succeeds.
    pub cancellable: bool,
    broken: Option<String>,
    /// A still-running call: polls left and its answer.
    pending: Option<(usize, Json)>,
    pub batches: usize,
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder { calls: Vec::new(), next: 100, fail: Vec::new(), rules: Vec::new(), missing_tools: Vec::new(), restarts: 0, cancellable: false, broken: None, pending: None, batches: 0 }
    }

    /// Script `action` for calls of `when` (a command id or a tool), `times` times (0: always).
    pub fn on(mut self, when: &str, action: Action, times: usize) -> Recorder {
        self.rules.push(Rule { when: when.into(), action, times });
        self
    }

    /// The commands run, in order (from `execute` calls and batch steps).
    pub fn commands(&self) -> Vec<String> {
        self.calls.iter().filter(|(t, _)| t != "batch").filter_map(|(_, a)| a.str_of("command").map(str::to_string)).collect()
    }

    fn rule_for(&mut self, key: &str) -> Option<Action> {
        let i = self.rules.iter().position(|r| r.when == key)?;
        let r = &mut self.rules[i];
        let action = r.action.clone();
        if r.times == 1 {
            self.rules.remove(i);
        } else if r.times > 1 {
            r.times -= 1;
        }
        Some(action)
    }

    fn default_answer(&mut self, cmd: &str) -> Json {
        self.next += 1;
        let n = self.next;
        match cmd {
            "component.insert_design" | "component.create" | "occurrence.copy" | "file.insert_step" => {
                obj! {"component" => n, "occurrence" => n + 1000, "created_bodies" => vec![format!("Body{n}")]}
            }
            "inspect.measure" => obj! {"bodies" => vec![obj!{"name" => format!("Body{n}")}]},
            "joint.find_limits" => obj! {"values" => vec![obj!{"index" => 0, "min" => -30.0, "max" => 45.0, "min_by" => obj!{"a" => "x"}, "max_by" => obj!{"a" => "y"}}]},
            _ => obj! {"ok" => true},
        }
    }

    /// One command (from `execute` or a batch step).
    fn command(&mut self, cmd: &str, timeout: Duration) -> Result<Json, CallError> {
        if self.fail.iter().any(|f| f == cmd) {
            return Err(CallError::Failed(format!("{cmd}: refused (test)")));
        }
        match self.rule_for(cmd) {
            None => Ok(self.default_answer(cmd)),
            Some(a) => self.act(a, cmd, timeout),
        }
    }

    fn act(&mut self, a: Action, what: &str, timeout: Duration) -> Result<Json, CallError> {
        match a {
            Action::Answer(v) => Ok(v),
            Action::Fail(e) => Err(CallError::Failed(e)),
            Action::Delay(d) if d <= timeout => {
                std::thread::sleep(d);
                Ok(self.default_answer(what))
            }
            Action::Delay(_) | Action::Hang => {
                std::thread::sleep(timeout);
                let why = format!("no answer from SolidCraft within {} s", timeout.as_secs_f64());
                self.broken = Some(why.clone());
                Err(CallError::TimedOut(why))
            }
            Action::Die => {
                self.broken = Some("the server stopped".into());
                Err(CallError::Lost("SolidCraft's MCP server stopped".into()))
            }
            Action::StillRunning(polls) => {
                let answer = self.default_answer(what);
                self.pending = Some((polls, answer));
                Err(CallError::Failed(format!("still running: `{what}` is still running in the app; it will still apply when it finishes (`pending` gets its result)")))
            }
        }
    }

    fn batch(&mut self, args: &Json, timeout: Duration) -> Result<Json, CallError> {
        self.batches += 1;
        let steps = args.arr_of("commands").to_vec();
        let mut results = Vec::new();
        let mut timings = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            let cmd = step.str_of("command").unwrap_or("").to_string();
            self.calls.push(("batch step".into(), obj! {"command" => cmd.as_str(), "params" => step.get("params").cloned().unwrap_or_default()}));
            match self.command(&cmd, timeout) {
                Ok(v) => {
                    results.push(v);
                    timings.push(obj! {"index" => i, "command" => cmd.as_str(), "ms" => 2.5});
                }
                Err(CallError::Failed(e)) => {
                    let reply = obj! {
                        "ok" => false, "completed" => i,
                        "failed" => obj!{"index" => i, "command" => cmd.as_str(), "error" => e.as_str()},
                        "rolled_back" => false, "undone" => 0,
                        "applied" => (0..i).collect::<Vec<_>>(), "not_run" => (i + 1..steps.len()).collect::<Vec<_>>(),
                        "results" => Json::Arr(results), "timings" => Json::Arr(timings),
                    };
                    return Err(CallError::Failed(reply.to_compact()));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(obj! {"ok" => true, "completed" => results.len(), "results" => Json::Arr(results), "timings" => Json::Arr(timings)})
    }
}

impl Default for Recorder {
    fn default() -> Recorder {
        Recorder::new()
    }
}

impl Transport for Recorder {
    fn call(&mut self, tool: &str, args: Json) -> Result<Json, String> {
        self.call_within(tool, args, Duration::from_secs(3600)).map_err(|e| e.message().to_string())
    }

    fn call_within(&mut self, tool: &str, args: Json, timeout: Duration) -> Result<Json, CallError> {
        if let Some(why) = &self.broken {
            return Err(CallError::Lost(format!("the SolidCraft session was given up earlier ({why})")));
        }
        self.calls.push((tool.to_string(), args.clone()));
        if self.missing_tools.iter().any(|t| t == tool) {
            return Err(CallError::Failed(format!("unknown tool `{tool}`")));
        }
        match tool {
            "execute" => {
                let cmd = args.str_of("command").unwrap_or("").to_string();
                self.command(&cmd, timeout)
            }
            "batch" => self.batch(&args, timeout),
            "pending" => match self.pending.take() {
                None => Ok(obj! {"pending" => Json::Null, "note" => "nothing is running"}),
                Some((0, answer)) => Ok(obj! {"pending" => "x", "done" => true, "result" => answer}),
                Some((n, answer)) => {
                    self.pending = Some((n - 1, answer));
                    Ok(obj! {"pending" => "x", "done" => false})
                }
            },
            "cancel" if self.cancellable => {
                self.pending = None;
                Ok(obj! {"cancelled" => true})
            }
            "cancel" => Err(CallError::Failed("it is already applying and can't be cancelled".into())),
            _ => match self.rule_for(tool) {
                Some(a) => self.act(a, tool, timeout),
                None => Ok(self.default_answer(tool)),
            },
        }
    }

    fn restart(&mut self) -> Result<(), String> {
        if self.rule_for("restart").is_some() {
            return Err("could not start SolidCraft again (test)".into());
        }
        self.restarts += 1;
        self.broken = None;
        self.pending = None;
        Ok(())
    }
}
