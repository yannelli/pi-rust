//! `bash` tool: run a shell command, stream output, keep the tail, save full output when truncated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::ContentBlock;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::output::{OutputAccumulator, OutputSnapshot};
use super::parse_args;
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
const SESSION_VARIABLES: [&str; 5] =
    ["PI_SESSION_ID", "PI_SESSION_FILE", "PI_PROVIDER", "PI_MODEL", "PI_REASONING_LEVEL"];
static SESSION_ENVIRONMENT: LazyLock<RwLock<BTreeMap<String, String>>> = LazyLock::new(Default::default);

/// Set the `PI_*` session variables exposed to commands (session id and file, provider, model, reasoning level).
pub fn set_session_environment(variables: BTreeMap<String, String>) {
    *SESSION_ENVIRONMENT.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        variables.into_iter().filter(|(name, _)| SESSION_VARIABLES.contains(&name.as_str())).collect();
}
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
/// How long to keep reading after the shell exits while descendants still hold the pipes.
const POST_EXIT_IDLE: Duration = Duration::from_millis(250);

pub struct BashTool {
    cwd: PathBuf,
    description: String,
    /// `shellPath` setting: run commands with this shell.
    shell_path: Option<PathBuf>,
    /// `shellCommandPrefix` setting: a line run before every command.
    command_prefix: Option<String>,
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    timeout: Option<f64>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        let description = format!(
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
            DEFAULT_MAX_BYTES / 1024
        );
        Self { cwd, description, shell_path: None, command_prefix: None }
    }

    /// Run commands with `shell` (`-c <command>`); a missing file fails each command.
    pub fn with_shell_path(mut self, shell: Option<PathBuf>) -> Self {
        self.shell_path = shell;
        self
    }

    /// Run `prefix` on its own line before every command, such as `shopt -s expand_aliases`.
    pub fn with_command_prefix(mut self, prefix: Option<String>) -> Self {
        self.command_prefix = prefix.filter(|prefix| !prefix.is_empty());
        self
    }

    fn shell(&self) -> anyhow::Result<PathBuf> {
        match &self.shell_path {
            Some(path) if path.exists() => Ok(path.clone()),
            Some(path) => bail!("Custom shell path not found: {}", path.display()),
            None => Ok(shell_program()),
        }
    }
}

fn shell_program() -> PathBuf {
    if Path::new("/bin/bash").exists() {
        return PathBuf::from("/bin/bash");
    }
    let on_path = std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).map(|dir| dir.join("bash")).find(|p| p.is_file()));
    on_path.unwrap_or_else(|| PathBuf::from("sh"))
}

fn resolve_timeout(timeout: Option<f64>) -> anyhow::Result<Option<Duration>> {
    let Some(secs) = timeout else { return Ok(None) };
    if !secs.is_finite() || secs <= 0.0 {
        bail!("Invalid timeout: must be a finite number of seconds");
    }
    if secs * 1000.0 > MAX_TIMEOUT_MS {
        bail!("Invalid timeout: maximum is {} seconds", MAX_TIMEOUT_MS / 1000.0);
    }
    Ok(Some(Duration::from_secs_f64(secs)))
}

fn spawn_reader(mut stream: impl AsyncRead + Unpin + Send + 'static, tx: mpsc::UnboundedSender<Vec<u8>>) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
}

/// Kill the whole process group (the child leads its own group), then the child itself.
/// Uses the shell's `kill` builtin because a standalone `kill` binary is not always installed.
async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new(shell_program())
            .args(["-c", &format!("kill -KILL -- -{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.start_kill();
}

fn format_output(acc: &OutputAccumulator, snap: &OutputSnapshot, empty_text: &str) -> (String, Option<Value>) {
    let t = &snap.truncation;
    let mut text = if snap.content.is_empty() { empty_text.to_string() } else { snap.content.clone() };
    if !t.truncated {
        return (text, None);
    }
    let path = snap.full_output_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    let start = t.total_lines - t.output_lines + 1;
    let end = t.total_lines;
    if t.last_line_partial {
        let line_size = format_size(acc.last_line_bytes());
        let shown = format_size(t.output_bytes);
        text.push_str(&format!("\n\n[Showing last {shown} of line {end} (line is {line_size}). Full output: {path}]"));
    } else if t.truncated_by == Some(TruncatedBy::Lines) {
        text.push_str(&format!("\n\n[Showing lines {start}-{end} of {}. Full output: {path}]", t.total_lines));
    } else {
        let limit = format_size(DEFAULT_MAX_BYTES);
        text.push_str(&format!(
            "\n\n[Showing lines {start}-{end} of {} ({limit} limit). Full output: {path}]",
            t.total_lines
        ));
    }
    (text, Some(json!({ "truncation": t, "fullOutputPath": path })))
}

fn update_details(snap: &OutputSnapshot) -> Value {
    let mut map = Map::new();
    if snap.truncation.truncated {
        map.insert("truncation".into(), json!(snap.truncation));
    }
    if let Some(path) = &snap.full_output_path {
        map.insert("fullOutputPath".into(), json!(path.display().to_string()));
    }
    Value::Object(map)
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() { status.to_string() } else { format!("{text}\n\n{status}") }
}

enum Outcome {
    Exited(i32),
    Aborted,
    TimedOut,
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args: BashArgs = parse_args("bash", args)?;
        let timeout = resolve_timeout(args.timeout)?;
        if cancel.is_cancelled() {
            bail!("Command aborted");
        }
        if !self.cwd.exists() {
            bail!("Working directory does not exist: {}\nCannot execute bash commands.", self.cwd.display());
        }

        let script = match &self.command_prefix {
            Some(prefix) => format!("{prefix}\n{}", args.command),
            None => args.command.clone(),
        };
        let mut command = tokio::process::Command::new(self.shell()?);
        for name in SESSION_VARIABLES {
            command.env_remove(name);
        }
        command
            .arg("-c")
            .arg(&script)
            .current_dir(&self.cwd)
            .envs(pi_ai::env::child_process_env())
            .envs(SESSION_ENVIRONMENT.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|e| anyhow!("Failed to start shell: {e}"))?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, tx);
        }

        on_update(AgentToolResult::default());
        let mut acc = OutputAccumulator::new("pi-bash");
        let deadline = timeout.map(|t| Instant::now() + t);
        let far_future = Instant::now() + Duration::from_secs(86_400 * 365);
        let mut last_update = Instant::now().checked_sub(UPDATE_THROTTLE).unwrap_or_else(Instant::now);
        let mut dirty = false;
        let mut status = None;
        let mut streams_done = false;
        let mut idle_deadline = far_future;

        let outcome = loop {
            if let (Some(code), true) = (status, streams_done) {
                break Outcome::Exited(code);
            }
            tokio::select! {
                _ = cancel.cancelled() => break Outcome::Aborted,
                _ = tokio::time::sleep_until(deadline.unwrap_or(far_future)) => break Outcome::TimedOut,
                chunk = rx.recv(), if !streams_done => match chunk {
                    Some(data) => {
                        acc.append(&data);
                        dirty = true;
                        if status.is_some() {
                            idle_deadline = Instant::now() + POST_EXIT_IDLE;
                        }
                    }
                    None => streams_done = true,
                },
                result = child.wait(), if status.is_none() => {
                    status = Some(exit_code(result?));
                    idle_deadline = Instant::now() + POST_EXIT_IDLE;
                }
                _ = tokio::time::sleep_until(idle_deadline), if status.is_some() => streams_done = true,
                _ = tokio::time::sleep_until(last_update + UPDATE_THROTTLE), if dirty => {
                    dirty = false;
                    last_update = Instant::now();
                    let snap = acc.snapshot(true);
                    let details = update_details(&snap);
                    on_update(AgentToolResult::text(snap.content).with_details(details));
                }
            }
        };

        if !matches!(outcome, Outcome::Exited(_)) {
            kill_tree(&mut child).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            while let Ok(Some(data)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                acc.append(&data);
            }
        }

        acc.finish();
        let snap = acc.snapshot(true);
        acc.close_temp_file();
        if dirty {
            on_update(AgentToolResult::text(snap.content.clone()).with_details(update_details(&snap)));
        }

        match outcome {
            Outcome::Aborted => {
                let (text, _) = format_output(&acc, &snap, "");
                Err(anyhow!(append_status(&text, "Command aborted")))
            }
            Outcome::TimedOut => {
                let (text, _) = format_output(&acc, &snap, "");
                let secs = args.timeout.unwrap_or_default();
                Err(anyhow!(append_status(&text, &format!("Command timed out after {secs} seconds"))))
            }
            Outcome::Exited(code) => {
                let (text, details) = format_output(&acc, &snap, "(no output)");
                if code != 0 {
                    let text = append_status(&text, &format!("Command exited with code {code}"));
                    return Ok(AgentToolResult {
                        content: vec![ContentBlock::text(text)],
                        details,
                        is_error: true,
                        ..Default::default()
                    });
                }
                Ok(AgentToolResult { content: vec![ContentBlock::text(text)], details, ..Default::default() })
            }
        }
    }
}
