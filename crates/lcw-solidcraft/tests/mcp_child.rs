//! The real MCP client against a real child process: this test binary run again as a tiny fake
//! MCP server (the `fake_server` test, which does nothing unless the environment asks it to).
//! It answers, fails, hangs or exits, so the client's timeout, its giving up of the session and
//! its restart are checked over real pipes.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use lcw_json::{obj, Json};
use lcw_solidcraft::mcp::{CallError, Launch, McpClient, Transport};

const MODE: &str = "INV2SC_FAKE_MCP";

#[test]
fn fake_server() {
    let Ok(mode) = std::env::var(MODE) else { return };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    // The test harness has written "test fake_server ... " without a line end: end its line.
    let _ = out.write_all(b"\n");
    let _ = out.flush();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = Json::parse(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue };
        let result = match msg.str_of("method") {
            Some("initialize") => obj! {"serverInfo" => obj!{"name" => "fake", "version" => "0"}},
            Some("tools/call") => {
                let cmd = msg.pointer("params/arguments/command").and_then(Json::as_str).unwrap_or("").to_string();
                match cmd.as_str() {
                    "slow.cmd" if mode == "hang" => loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    },
                    "die.cmd" => std::process::exit(3),
                    "bad.cmd" => obj! {"isError" => true, "content" => vec![obj!{"type" => "text", "text" => "bad.cmd: refused"}]},
                    _ => obj! {"content" => vec![obj!{"type" => "text", "text" => obj!{"ok" => true, "ran" => cmd.as_str()}.to_compact()}]},
                }
            }
            _ => Json::obj(),
        };
        let reply = obj! {"jsonrpc" => "2.0", "id" => id, "result" => result};
        let _ = out.write_all(format!("{}\n", reply.to_compact()).as_bytes());
        let _ = out.flush();
    }
    std::process::exit(0);
}

fn launch(mode: &str) -> Launch {
    let exe = std::env::current_exe().unwrap().display().to_string();
    let mut l = Launch::new(&exe, &["fake_server".into(), "--exact".into(), "--nocapture".into(), "--test-threads=1".into()]);
    l.env.push((MODE.into(), mode.into()));
    l
}

fn run(c: &mut McpClient, cmd: &str, timeout: Duration) -> Result<Json, CallError> {
    c.call_within("execute", obj! {"command" => cmd, "params" => Json::obj()}, timeout)
}

#[test]
fn a_call_that_never_answers_times_out_and_the_session_restarts() {
    let mut c = McpClient::start(launch("hang"), Duration::from_secs(20)).expect("fake server starts");
    assert_eq!(run(&mut c, "fast.cmd", Duration::from_secs(20)).unwrap().str_of("ran"), Some("fast.cmd"));
    let t = Instant::now();
    let e = run(&mut c, "slow.cmd", Duration::from_millis(300)).unwrap_err();
    assert!(matches!(&e, CallError::TimedOut(m) if m.contains("no answer")), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(10), "gave up in time: {:?}", t.elapsed());
    // The session is given up: nothing more is sent into it.
    assert!(matches!(run(&mut c, "fast.cmd", Duration::from_secs(5)), Err(CallError::Lost(_))));
    c.restart().expect("restarts");
    assert_eq!(run(&mut c, "fast.cmd", Duration::from_secs(20)).unwrap().str_of("ran"), Some("fast.cmd"));
}

#[test]
fn a_server_that_exits_is_a_lost_session_and_an_error_is_just_an_error() {
    let mut c = McpClient::start(launch("plain"), Duration::from_secs(20)).expect("fake server starts");
    assert_eq!(run(&mut c, "bad.cmd", Duration::from_secs(20)), Err(CallError::Failed("bad.cmd: refused".into())));
    assert!(run(&mut c, "fast.cmd", Duration::from_secs(20)).is_ok(), "an error leaves the session usable");
    let t = Instant::now();
    let e = run(&mut c, "die.cmd", Duration::from_secs(20)).unwrap_err();
    assert!(matches!(&e, CallError::Lost(m) if m.contains("stopped")), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(10), "noticed at once, not at the timeout");
}
