//! The `command` status line: runs the user's command with session JSON on stdin and shows its
//! output, like Claude Code. Runs are debounced, a new run cancels the one in flight, and a slow
//! command keeps the last output on screen.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::{Rc, Weak};
use std::time::Duration;

use pi_tui::ui_loop::{self, TimerId};
use pi_tui::utils::truncate_to_width_with;
use tokio::io::AsyncWriteExt;

use super::{StatusSnapshot, status_payload};
use crate::tools::CommandTransport;

const DEBOUNCE: Duration = Duration::from_millis(300);
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LINES: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct CommandConfig {
    pub command: String,
    pub padding: usize,
    /// Also rerun on this interval (`refreshInterval`, at least one second).
    pub refresh: Option<Duration>,
    pub shell_path: Option<PathBuf>,
}

#[derive(Default)]
struct State {
    output: Option<Vec<String>>,
    snapshot: Option<StatusSnapshot>,
    payload: String,
    columns: usize,
    debounce: Option<TimerId>,
    running: Option<tokio::task::AbortHandle>,
    generation: u64,
}

pub struct CommandStatusLine {
    config: CommandConfig,
    state: Rc<RefCell<State>>,
    request_render: Rc<dyn Fn()>,
    refresh: Option<TimerId>,
}

impl CommandStatusLine {
    pub fn new(config: CommandConfig, request_render: Rc<dyn Fn()>) -> Self {
        let state = Rc::new(RefCell::new(State::default()));
        let refresh = config.refresh.map(|period| {
            let (weak, config, render) = (Rc::downgrade(&state), config.clone(), request_render.clone());
            ui_loop::set_interval(period, move || {
                if weak.upgrade().is_some_and(|state| state.borrow().snapshot.is_some()) {
                    run(&weak, &config, &render);
                }
            })
        });
        Self { config, state, request_render, refresh }
    }

    pub fn config(&self) -> &CommandConfig {
        &self.config
    }

    /// Schedules a run when the session data or the width changed; the first run starts at once.
    pub fn observe(&self, snapshot: &StatusSnapshot, columns: usize) {
        let first = {
            let mut state = self.state.borrow_mut();
            if state.snapshot.as_ref() == Some(snapshot) && state.columns == columns {
                return;
            }
            let first = state.snapshot.is_none();
            state.payload = status_payload(snapshot).to_string();
            state.snapshot = Some(snapshot.clone());
            state.columns = columns;
            first
        };
        let (weak, config, render) = (Rc::downgrade(&self.state), self.config.clone(), self.request_render.clone());
        let id =
            ui_loop::set_timeout(if first { Duration::ZERO } else { DEBOUNCE }, move || run(&weak, &config, &render));
        if let Some(old) = self.state.borrow_mut().debounce.replace(id) {
            ui_loop::clear_timer(old);
        }
    }

    /// The latest output fitted to `width`; `None` until the first run finishes.
    pub fn lines(&self, width: usize) -> Option<Vec<String>> {
        let state = self.state.borrow();
        let output = state.output.as_ref()?;
        let padding = self.config.padding.min(width);
        let pad = " ".repeat(padding);
        let fit = |line: &String| format!("{pad}{}", truncate_to_width_with(line, width - padding, "…", false));
        Some(output.iter().map(fit).collect())
    }
}

impl Drop for CommandStatusLine {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        for id in [self.refresh.take(), state.debounce.take()].into_iter().flatten() {
            ui_loop::clear_timer(id);
        }
        if let Some(running) = state.running.take() {
            running.abort();
        }
    }
}

fn run(state: &Weak<RefCell<State>>, config: &CommandConfig, request_render: &Rc<dyn Fn()>) {
    let Some(shared) = state.upgrade() else { return };
    let (payload, columns, cwd, generation) = {
        let mut state = shared.borrow_mut();
        state.debounce = None;
        if let Some(running) = state.running.take() {
            running.abort();
        }
        state.generation += 1;
        let cwd = state.snapshot.as_ref().map(|snapshot| snapshot.cwd.clone()).unwrap_or_default();
        (state.payload.clone(), state.columns, cwd, state.generation)
    };
    let task = tokio::spawn(run_command(config.clone(), payload, columns, cwd));
    shared.borrow_mut().running = Some(task.abort_handle());
    let (weak, render) = (state.clone(), request_render.clone());
    tokio::task::spawn_local(async move {
        let Ok(Some(lines)) = task.await else { return };
        let Some(shared) = weak.upgrade() else { return };
        {
            let mut state = shared.borrow_mut();
            if state.generation != generation {
                return;
            }
            state.running = None;
            state.output = Some(lines);
        }
        render();
    });
}

/// Runs `command` in the bash shell with `payload` on stdin; `None` when it fails to start or times out.
pub(super) async fn run_command(
    config: CommandConfig,
    payload: String,
    columns: usize,
    cwd: PathBuf,
) -> Option<Vec<String>> {
    let shell = crate::tools::bash_shell_config(config.shell_path.as_deref()).ok()?;
    let mut process = tokio::process::Command::new(&shell.shell);
    process.args(&shell.args);
    // A shell that reads its script from stdin gets the JSON as a here-document instead.
    let input = if shell.transport == CommandTransport::Stdin {
        format!("{} <<'PI_STATUS_LINE_JSON'\n{payload}\nPI_STATUS_LINE_JSON\n", config.command)
    } else {
        process.arg(&config.command);
        format!("{payload}\n")
    };
    if Path::new(&cwd).is_dir() {
        process.current_dir(&cwd);
    }
    process
        .envs(pi_ai::env::child_process_env())
        .env("COLUMNS", columns.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = process.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }
    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output_lines(&String::from_utf8_lossy(&output.stdout)))
}

/// Output lines without trailing blank lines, tabs as spaces, and at most [`MAX_LINES`].
pub(super) fn output_lines(stdout: &str) -> Vec<String> {
    let mut lines: Vec<String> =
        stdout.lines().take(MAX_LINES).map(|line| line.trim_end_matches('\r').replace('\t', "    ")).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}
