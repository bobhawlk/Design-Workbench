//! Progress while building: a line per step on stderr (throttled), a machine-readable progress
//! file rewritten as the run goes, and a build log appended and flushed as each step finishes,
//! so a hang or a crash keeps everything up to the step it stopped at.
//!
//! The progress file (`lcw-progress/1`) is one JSON object, replaced whole (written next to it,
//! then renamed over it) so a reader never sees half of it:
//! `{schema, state: running|finished, phase, design, design_index, design_total, step,
//! steps_total, label, elapsed_s, failures, design_failures}`.
//! The log (`lcw-build-log/1`) is JSON lines: `design`, `step` (with `status`, `detail`,
//! `command`, `ms`), `design_done`, `event` and `run_done` records.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lcw_json::{obj, Json};

use crate::run::{clip, DesignResult, Observer, Status, StepResult};
use crate::steps::DesignSteps;

/// A step slower than this is always printed, so the slow commands show.
const SLOW: Duration = Duration::from_secs(5);

pub struct ProgressSink {
    /// Print progress lines on stderr.
    pub print: bool,
    /// Print every step, not at most one per `interval`.
    pub verbose: bool,
    pub interval: Duration,
    progress_file: Option<PathBuf>,
    log: Option<File>,
    started: Instant,
    last_print: Option<Instant>,
    last_write: Option<Instant>,
    /// Which design (counted from 0) is the assembly, to name the phase.
    pub assembly_index: Option<usize>,
    pub design_total: usize,
    phase: String,
    design: String,
    design_index: usize,
    step: usize,
    steps_total: usize,
    label: String,
    failures: usize,
    design_failures: usize,
    finished: bool,
    /// Writing the log or the progress file failed (said once).
    pub io_error: Option<String>,
    /// Lines printed (for tests).
    pub printed: usize,
}

impl ProgressSink {
    /// `log`: the build log, started afresh. `progress_file`: rewritten as the run goes.
    pub fn new(progress_file: Option<PathBuf>, log: Option<PathBuf>) -> Result<ProgressSink, String> {
        let log = match log {
            Some(p) => Some(OpenOptions::new().create(true).write(true).truncate(true).open(&p).map_err(|e| format!("{}: {e}", p.display()))?),
            None => None,
        };
        let mut s = ProgressSink {
            print: true,
            verbose: false,
            interval: Duration::from_millis(250),
            progress_file,
            log,
            started: Instant::now(),
            last_print: None,
            last_write: None,
            assembly_index: None,
            design_total: 0,
            phase: "starting".into(),
            design: String::new(),
            design_index: 0,
            step: 0,
            steps_total: 0,
            label: String::new(),
            failures: 0,
            design_failures: 0,
            finished: false,
            io_error: None,
            printed: 0,
        };
        s.write_progress(true);
        Ok(s)
    }

    fn elapsed(&self) -> f64 {
        (self.started.elapsed().as_secs_f64() * 10.0).round() / 10.0
    }

    /// Name what the run is doing now ("probe", "parts", "assembly"…).
    pub fn set_phase(&mut self, phase: &str) {
        self.phase = phase.into();
        self.write_progress(true);
    }

    /// The run is over.
    pub fn finish(&mut self, ok: bool) {
        self.finished = true;
        self.phase = "done".into();
        let j = obj! {"event" => "run_done", "ok" => ok, "failures" => self.failures, "t" => self.elapsed()};
        self.log_line(&j);
        self.write_progress(true);
    }

    pub fn progress_json(&self) -> Json {
        obj! {
            "schema" => "lcw-progress/1",
            "state" => if self.finished { "finished" } else { "running" },
            "phase" => self.phase.as_str(),
            "design" => self.design.as_str(),
            "design_index" => self.design_index,
            "design_total" => self.design_total,
            "step" => self.step,
            "steps_total" => self.steps_total,
            "label" => self.label.as_str(),
            "elapsed_s" => self.elapsed(),
            "failures" => self.failures,
            "design_failures" => self.design_failures,
        }
    }

    fn write_progress(&mut self, force: bool) {
        let Some(path) = self.progress_file.clone() else { return };
        if !force && self.last_write.is_some_and(|t| t.elapsed() < self.interval) {
            return;
        }
        self.last_write = Some(Instant::now());
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let text = self.progress_json().to_pretty();
        if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
            self.io_failed(format!("{}: {e}", path.display()));
        }
    }

    fn log_line(&mut self, j: &Json) {
        let Some(f) = self.log.as_mut() else { return };
        let mut line = j.to_compact();
        line.push('\n');
        if let Err(e) = f.write_all(line.as_bytes()).and_then(|_| f.flush()) {
            self.io_failed(format!("build log: {e}"));
        }
    }

    fn io_failed(&mut self, e: String) {
        if self.io_error.is_none() {
            eprintln!("warning: {e}");
            self.io_error = Some(e);
        }
    }
}

impl Observer for ProgressSink {
    fn design_started(&mut self, design: &DesignSteps) {
        self.design_index += 1;
        if let Some(a) = self.assembly_index {
            self.phase = if self.design_index > a { "assembly".into() } else { "parts".into() };
        }
        self.design = design.name.clone();
        self.step = 0;
        self.steps_total = design.steps.len();
        self.label.clear();
        self.design_failures = 0;
        let j = obj! {"event" => "design", "design" => design.name.as_str(), "file" => design.file.as_str(), "index" => self.design_index, "total" => self.design_total, "steps" => design.steps.len(), "t" => self.elapsed()};
        self.log_line(&j);
        if self.print {
            eprintln!("[{}/{}] {} ({} steps)", self.design_index, self.design_total.max(self.design_index), design.name, design.steps.len());
            self.printed += 1;
        }
        self.write_progress(true);
    }

    fn step_finished(&mut self, design: &DesignSteps, index: usize, r: &StepResult) {
        self.step = index + 1;
        self.label = r.label.clone();
        let failed = matches!(r.status, Status::Failed(_));
        if failed {
            self.failures += 1;
            self.design_failures += 1;
        }
        let mut j = obj! {
            "event" => "step", "design" => design.name.as_str(), "index" => index + 1, "total" => design.steps.len(),
            "label" => r.label.as_str(), "group" => r.group.as_str(), "op" => r.op.as_str(), "command" => r.command.as_str(),
            "status" => r.status.name(), "ms" => (r.ms * 10.0).round() / 10.0, "calls" => r.calls, "t" => self.elapsed(),
        };
        if !r.status.detail().is_empty() {
            j.set("detail", r.status.detail());
        }
        if !r.note.is_empty() {
            j.set("note", r.note.as_str());
        }
        if r.batched {
            j.set("batched", true);
        }
        self.log_line(&j);
        if self.print {
            let last = index + 1 == design.steps.len();
            let due = self.last_print.is_none_or(|t| t.elapsed() >= self.interval);
            let slow = r.ms >= SLOW.as_secs_f64() * 1000.0;
            if self.verbose || failed || slow || last || due {
                let pct = (100 * (index + 1)) / design.steps.len().max(1);
                eprintln!("  [{} {}/{} {pct}%] {} ({}, {:.1} s)", self.phase, index + 1, design.steps.len(), r.label, r.status.name(), r.ms / 1000.0);
                self.printed += 1;
                self.last_print = Some(Instant::now());
                if failed || (self.verbose && !r.status.detail().is_empty()) {
                    eprintln!("      {}", clip(r.status.detail().lines().next().unwrap_or(""), 400));
                }
            }
        }
        self.write_progress(failed);
    }

    fn design_finished(&mut self, r: &DesignResult) {
        let mut j = obj! {
            "event" => "design_done", "design" => r.name.as_str(), "saved" => r.saved, "ms" => r.ms.round(),
            "done" => r.count("done"), "fallback" => r.count("fallback"), "skipped" => r.count("skipped"), "failed" => r.count("failed"), "not_run" => r.count("not run"),
            "t" => self.elapsed(),
        };
        if let Some(s) = &r.stopped {
            j.set("stopped", s.as_str());
        }
        if let Some(s) = &r.session_lost {
            j.set("session_lost", s.as_str());
        }
        self.log_line(&j);
        if self.print {
            eprintln!("  {}: {}", r.name, r.summary());
            self.printed += 1;
        }
        self.write_progress(true);
    }

    fn event(&mut self, message: &str) {
        let j = obj! {"event" => "event", "message" => message, "t" => self.elapsed()};
        self.log_line(&j);
        if self.print {
            eprintln!("  {message}");
        }
    }
}
