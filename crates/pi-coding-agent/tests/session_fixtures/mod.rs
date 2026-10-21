//! Session files for the `pi sessions` and `pi stats` tests, written straight into a temporary
//! agent directory, and a runner for the `pi` binary with no keys and no network.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use pi_coding_agent::session::{format_iso, session_dir};
use serde_json::{Value, json};

use crate::cli_support::isolate;

pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 24 * HOUR;

pub fn now() -> i64 {
    pi_ai::now_ms()
}

/// Noon UTC `days` days before today, so the local day is the same in most time zones.
pub fn noon_days_ago(days: i64) -> i64 {
    (now() / DAY - days) * DAY + 12 * HOUR
}

/// `YYYY-MM-DD` of `millis` in UTC.
pub fn utc_date(millis: i64) -> String {
    format_iso(millis)[..10].to_string()
}

/// One prompt and its answer.
#[derive(Clone, Debug)]
pub struct Turn {
    pub provider: &'static str,
    pub model: &'static str,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

pub fn turn(provider: &'static str, model: &'static str, input: u64, output: u64, cost: f64) -> Turn {
    Turn { provider, model, input, output, cache_read: 0, cache_write: 0, cost }
}

/// A session to write: its id, project, first message time, prompt text, name, and turns.
#[derive(Clone, Debug)]
pub struct Spec {
    pub id: &'static str,
    pub cwd: PathBuf,
    pub at: i64,
    pub prompt: &'static str,
    pub name: Option<&'static str>,
    pub turns: Vec<Turn>,
}

impl Spec {
    pub fn new(id: &'static str, cwd: &Path, at: i64) -> Self {
        let turns = vec![turn("anthropic", "claude-test", 100, 10, 0.01)];
        Self { id, cwd: cwd.to_path_buf(), at, prompt: "hello", name: None, turns }
    }

    pub fn lines(&self) -> Vec<Value> {
        let header = json!({"type": "session", "version": 3, "id": self.id, "timestamp": format_iso(self.at),
            "cwd": self.cwd.display().to_string()});
        let mut lines = vec![header];
        let mut parent = Value::Null;
        let mut push = |lines: &mut Vec<Value>, mut entry: Value, time: i64| {
            let id = format!("e{}", lines.len());
            entry["id"] = json!(id);
            entry["parentId"] = parent.clone();
            entry["timestamp"] = json!(format_iso(time));
            parent = json!(id);
            lines.push(entry);
        };
        if let Some(name) = self.name {
            push(&mut lines, json!({"type": "session_info", "name": name}), self.at);
        }
        for (index, turn) in self.turns.iter().enumerate() {
            let asked = self.at + index as i64 * 60_000;
            let prompt = if index == 0 { self.prompt.to_string() } else { format!("follow-up {index}") };
            let user = json!({"role": "user", "content": [{"type": "text", "text": prompt}], "timestamp": asked});
            push(&mut lines, json!({"type": "message", "message": user}), asked);
            let usage = json!({"input": turn.input, "output": turn.output, "cacheRead": turn.cache_read,
                "cacheWrite": turn.cache_write, "totalTokens": turn.input + turn.output,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": turn.cost}});
            let answer = json!({"role": "assistant", "content": [{"type": "text", "text": "done"}],
                "api": "openai-completions", "provider": turn.provider, "model": turn.model, "usage": usage,
                "stopReason": "stop", "timestamp": asked + 1_000});
            push(&mut lines, json!({"type": "message", "message": answer}), asked + 1_000);
        }
        lines
    }

    /// Write the session into `dir` and return its path.
    pub fn write_to(&self, dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let stamp = format_iso(self.at).replace([':', '.'], "-");
        let path = dir.join(format!("{stamp}_{}.jsonl", self.id));
        let text: String = self.lines().iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&path, text).unwrap();
        path
    }
}

/// A temporary root with `agent/`, and the projects `work/` (where `pi` runs) and `other/`.
pub struct Root {
    pub dir: tempfile::TempDir,
}

impl Root {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for name in ["agent", "work", "other"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        Self { dir }
    }

    pub fn agent(&self) -> PathBuf {
        self.dir.path().join("agent")
    }

    /// A project directory as the spawned `pi` sees it: on macOS the temp directory is a symlink.
    pub fn project(&self, name: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        if cfg!(target_os = "macos") { std::fs::canonicalize(&path).unwrap() } else { path }
    }

    pub fn work(&self) -> PathBuf {
        self.project("work")
    }

    pub fn other(&self) -> PathBuf {
        self.project("other")
    }

    /// Write `spec` into the default session folder of its project.
    pub fn write(&self, spec: &Spec) -> PathBuf {
        spec.write_to(&session_dir(&self.agent(), &spec.cwd))
    }

    /// `pi <args>` in `work/`, in UTC, with stdin closed.
    pub fn pi(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        isolate(&mut command, self.dir.path())
            .args(args)
            .current_dir(self.dir.path().join("work"))
            .env_remove("PI_CODING_AGENT_SESSION_DIR")
            .env("TZ", "UTC")
            .stdin(Stdio::null());
        command.output().unwrap()
    }
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Exit code 0, or a panic that shows both streams.
pub fn ok(output: Output) -> Output {
    assert_eq!(output.status.code(), Some(0), "stdout: {}\nstderr: {}", stdout(&output), stderr(&output));
    output
}

pub fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {}", stdout(output)))
}
