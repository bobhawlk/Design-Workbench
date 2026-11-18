//! A Model Context Protocol client for `solidcraft-cli mcp` over stdio.
//!
//! The MCP server is SolidCraft's own, supported interface for programs: the same commands the UI
//! runs, with a catalogue and JSON Schemas. Headless (`solidcraft-cli mcp`) it builds designs in
//! its own session; with `--connect HOST:PORT` it drives the running app, so the user watches the
//! assembly being built.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use lcw_json::{obj, Json};

/// What the runner needs from a SolidCraft session.
pub trait Transport {
    /// Call an MCP tool; Ok is the tool's result (its text content parsed as JSON when it is
    /// JSON), Err the tool's or the protocol's error message.
    fn call(&mut self, tool: &str, args: Json) -> Result<Json, String>;
}

pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    pub timeout: Duration,
    pub server_info: Json,
}

impl McpClient {
    /// Start `program args…` and initialise the MCP session.
    pub fn spawn(program: &str, args: &[String]) -> Result<McpClient, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("could not start `{program}`: {e} (is SolidCraft installed? pass --solidcraft-cli PATH)"))?;
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
        let mut c = McpClient { child, stdin, lines: rx, next_id: 1, timeout: Duration::from_secs(1800), server_info: Json::Null };
        let init = c.request(
            "initialize",
            obj! {"protocolVersion" => "2025-06-18", "capabilities" => Json::obj(), "clientInfo" => obj!{"name" => "inv2sc", "version" => env!("CARGO_PKG_VERSION")}},
        )?;
        c.server_info = init.get("serverInfo").cloned().unwrap_or_default();
        c.notify("notifications/initialized", Json::obj())?;
        Ok(c)
    }

    fn send(&mut self, msg: &Json) -> Result<(), String> {
        let mut line = msg.to_compact();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()).map_err(|e| format!("SolidCraft's MCP server closed its input: {e}"))
    }

    fn notify(&mut self, method: &str, params: Json) -> Result<(), String> {
        self.send(&obj! {"jsonrpc" => "2.0", "method" => method, "params" => params})
    }

    pub fn request(&mut self, method: &str, params: Json) -> Result<Json, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&obj! {"jsonrpc" => "2.0", "id" => id, "method" => method, "params" => params})?;
        loop {
            let line = self.lines.recv_timeout(self.timeout).map_err(|_| format!("no answer from SolidCraft to `{method}` within {:?}", self.timeout))?;
            let Ok(msg) = Json::parse(&line) else { continue };
            // Server requests and notifications (logging, progress) are not ours.
            if msg.get("id").and_then(Json::as_u64) != Some(id) || msg.get("method").is_some() {
                continue;
            }
            if let Some(e) = msg.get("error") {
                return Err(e.str_of("message").unwrap_or("MCP error").to_string());
            }
            return Ok(msg.get("result").cloned().unwrap_or_default());
        }
    }
}

impl Transport for McpClient {
    fn call(&mut self, tool: &str, args: Json) -> Result<Json, String> {
        let r = self.request("tools/call", obj! {"name" => tool, "arguments" => args})?;
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
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A transport that answers every call from a script of canned results (tests, dry runs).
pub struct Recorder {
    pub calls: Vec<(String, Json)>,
    next: u64,
    /// Commands to fail (by id), for testing fallbacks.
    pub fail: Vec<String>,
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder { calls: Vec::new(), next: 100, fail: Vec::new() }
    }
}

impl Default for Recorder {
    fn default() -> Recorder {
        Recorder::new()
    }
}

impl Transport for Recorder {
    fn call(&mut self, tool: &str, args: Json) -> Result<Json, String> {
        self.calls.push((tool.to_string(), args.clone()));
        let cmd = args.str_of("command").unwrap_or("").to_string();
        if self.fail.contains(&cmd) {
            return Err(format!("{cmd}: refused (test)"));
        }
        self.next += 1;
        let n = self.next;
        Ok(match cmd.as_str() {
            "component.insert_design" | "component.create" | "occurrence.copy" | "file.insert_step" => {
                obj! {"component" => n, "occurrence" => n + 1000, "created_bodies" => vec![format!("Body{n}")]}
            }
            "inspect.measure" => obj! {"bodies" => vec![obj!{"name" => format!("Body{n}")}]},
            "joint.find_limits" => obj! {"values" => vec![obj!{"index" => 0, "min" => -30.0, "max" => 45.0, "min_by" => obj!{"a" => "x"}, "max_by" => obj!{"a" => "y"}}]},
            _ => obj! {"ok" => true},
        })
    }
}
