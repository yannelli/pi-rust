//! Starting an ACP agent: the command of its [`AgentLaunch`] in the session directory, with
//! JSON-RPC on stdin and stdout and the last lines of stderr kept for error messages.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use super::connection::Connection;
use super::host::AgentLaunch;

const STDERR_LINES: usize = 20;

/// A running agent; dropping it stops the process.
pub(crate) struct Process {
    pub connection: Arc<Connection>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    child: Child,
}

impl Process {
    /// The last lines the agent wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        let lines = self.stderr.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// Stops the agent and the processes it started, and waits for it to exit.
    pub async fn shutdown(&mut self) {
        kill_group(&self.child);
        let _ = self.child.kill().await;
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        kill_group(&self.child);
    }
}

/// Kills the process group of the agent, which `spawn` made with the agent as its leader. A
/// reaped child has no ID, so pi never signals a reused one.
fn kill_group(child: &Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()).and_then(rustix::process::Pid::from_raw) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// Leaks one label per agent name; agents come from settings, so the set is small.
fn label(name: &str) -> &'static str {
    static LABELS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut labels = LABELS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(label) = labels.iter().find(|label| **label == name) {
        return label;
    }
    let label: &'static str = Box::leak(name.to_string().into_boxed_str());
    labels.push(label);
    label
}

pub(crate) fn spawn(agent: &AgentLaunch, cwd: &Path) -> Result<Process, String> {
    let mut command = Command::new(&agent.command);
    command
        .args(&agent.args)
        .envs(&agent.env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child =
        command.spawn().map_err(|error| format!("Cannot start {} ({}): {error}", agent.name, agent.command))?;
    let missing = || format!("{} has no stdio pipes", agent.name);
    let stdin = child.stdin.take().ok_or_else(missing)?;
    let stdout = child.stdout.take().ok_or_else(missing)?;
    let stderr = child.stderr.take().ok_or_else(missing)?;
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let lines = tail.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let mut lines = lines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if lines.len() == STDERR_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    });
    let connection = Connection::new(label(&agent.name), stdout, stdin);
    Ok(Process { connection, stderr: tail, child })
}
