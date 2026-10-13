//! Shell tools (`bash`, `powershell`): run a command, stream output, keep the tail, and save
//! the full output when truncated. Port of upstream `createShellToolDefinition`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::ContentBlock;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::output::{OutputAccumulator, OutputSnapshot};
use super::parse_args;
use super::shell_process::{ShellExecError, ShellExecOptions, ShellOperations, shell_env};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size};

/// Limit of `structured_content.output`, which programmatic callers such as codemode receive.
const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
const SESSION_VARIABLES: [&str; 5] =
    ["PI_SESSION_ID", "PI_SESSION_FILE", "PI_PROVIDER", "PI_MODEL", "PI_REASONING_LEVEL"];
static SESSION_ENVIRONMENT: LazyLock<RwLock<BTreeMap<String, String>>> = LazyLock::new(Default::default);

/// Set the `PI_*` session variables exposed to commands (session id and file, provider, model, reasoning level).
pub fn set_session_environment(variables: BTreeMap<String, String>) {
    *SESSION_ENVIRONMENT.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        variables.into_iter().filter(|(name, _)| SESSION_VARIABLES.contains(&name.as_str())).collect();
}

/// The variables last passed to [`set_session_environment`].
pub fn session_environment() -> BTreeMap<String, String> {
    SESSION_ENVIRONMENT.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// What distinguishes one shell tool from another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellToolConfig {
    pub name: &'static str,
    pub label: &'static str,
    /// Shell name in the description and errors.
    pub shell_name: &'static str,
    /// Prompt shown before the command when rendering calls, such as `$`.
    pub prompt: &'static str,
    pub prompt_snippet: &'static str,
    /// Guidelines about the `PI_*` variables; left out when they are not exposed.
    pub prompt_guidelines: &'static [&'static str],
    pub temp_file_prefix: &'static str,
}

/// The command, directory, and full environment a command runs with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSpawnContext {
    pub command: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// Adjusts the command, directory, or environment before each command runs.
pub type ShellSpawnHook = Arc<dyn Fn(ShellSpawnContext) -> ShellSpawnContext + Send + Sync>;
/// Supplies the `PI_*` session variables; the default reads [`set_session_environment`].
pub type SessionEnvironmentFn = Arc<dyn Fn() -> BTreeMap<String, String> + Send + Sync>;

/// A shell tool. `bash` and `powershell` are both this type with different configs.
pub struct ShellTool {
    pub(super) config: ShellToolConfig,
    pub(super) cwd: PathBuf,
    description: String,
    pub(super) operations: Arc<dyn ShellOperations>,
    pub(super) command_prefix: Option<String>,
    pub(super) expose_session_environment: bool,
    pub(super) spawn_hook: Option<ShellSpawnHook>,
    pub(super) session_environment: Option<SessionEnvironmentFn>,
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    timeout: Option<f64>,
}

impl ShellTool {
    pub fn with_config(cwd: PathBuf, config: ShellToolConfig, operations: Arc<dyn ShellOperations>) -> Self {
        let description = format!(
            "Execute a {} command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
            config.shell_name,
            DEFAULT_MAX_BYTES / 1024
        );
        Self {
            config,
            cwd,
            description,
            operations,
            command_prefix: None,
            expose_session_environment: true,
            spawn_hook: None,
            session_environment: None,
        }
    }

    pub fn config(&self) -> &ShellToolConfig {
        &self.config
    }

    /// The context a command runs with: the shell environment without inherited `PI_*`
    /// session variables, then the session's own when exposed, then the spawn hook.
    pub fn spawn_context(&self, command: &str) -> ShellSpawnContext {
        let command = match &self.command_prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };
        let mut env = shell_env();
        env.retain(|name, _| !SESSION_VARIABLES.contains(&name.as_str()));
        if self.expose_session_environment {
            let session = match &self.session_environment {
                Some(provider) => provider(),
                None => session_environment(),
            };
            env.extend(session.into_iter().filter(|(name, _)| SESSION_VARIABLES.contains(&name.as_str())));
        }
        let context = ShellSpawnContext { command, cwd: self.cwd.clone(), env };
        match &self.spawn_hook {
            Some(hook) => hook(context),
            None => context,
        }
    }
}

/// Output state shared with the streaming callback.
struct Stream {
    acc: OutputAccumulator,
    accepting: bool,
    dirty: bool,
    last_update: Option<Instant>,
}

fn update_result(snap: &OutputSnapshot) -> AgentToolResult {
    let mut map = Map::new();
    if snap.truncation.truncated {
        map.insert("truncation".into(), json!(snap.truncation));
    }
    if let Some(path) = &snap.full_output_path {
        map.insert("fullOutputPath".into(), json!(path.display().to_string()));
    }
    AgentToolResult::text(snap.content.clone()).with_details(Value::Object(map))
}

/// The update to emit, if output arrived since the last one.
fn take_update(stream: &Mutex<Stream>, force: bool) -> Option<AgentToolResult> {
    let mut stream = stream.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let due = stream.last_update.is_none_or(|last| last.elapsed() >= UPDATE_THROTTLE);
    if !stream.dirty || !(force || due) {
        return None;
    }
    stream.dirty = false;
    stream.last_update = Some(Instant::now());
    Some(update_result(&stream.acc.snapshot(true)))
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

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() { status.to_string() } else { format!("{text}\n\n{status}") }
}

/// A JSON number without a fraction when it is whole, as JavaScript writes it.
fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 1e15 { json!(value as i64) } else { json!(value) }
}

/// Schema of the shell tools' `structured_content`.
pub fn shell_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "output": { "type": "string", "description": "Combined stdout and stderr, possibly truncated" },
            "truncated": { "type": "boolean" },
            "full_output_path": { "type": "string", "description": "Full output, when truncated" },
            "exit_code": { "type": "number" },
            "wall_time_seconds": { "type": "number" }
        },
        "required": ["output", "truncated", "exit_code", "wall_time_seconds"]
    })
}

#[async_trait]
impl AgentTool for ShellTool {
    fn name(&self) -> &str {
        self.config.name
    }

    fn label(&self) -> &str {
        self.config.label
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

    fn output_schema(&self) -> Option<Value> {
        Some(shell_output_schema())
    }

    fn constrained_sampling(&self) -> Option<Value> {
        Some(super::prefer_json_schema())
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(self.config.prompt_snippet)
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        if !self.expose_session_environment {
            return Vec::new();
        }
        self.config.prompt_guidelines.iter().map(|guideline| guideline.to_string()).collect()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args: ShellArgs = parse_args(self.config.name, args)?;
        let spawn = self.spawn_context(&args.command);
        let stream = Arc::new(Mutex::new(Stream {
            acc: OutputAccumulator::new(self.config.temp_file_prefix),
            accepting: true,
            dirty: false,
            last_update: None,
        }));
        on_update(AgentToolResult::default());
        let on_data = {
            let (stream, on_update) = (stream.clone(), on_update.clone());
            Arc::new(move |data: &[u8]| {
                {
                    let mut stream = stream.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if !stream.accepting {
                        return;
                    }
                    stream.acc.append(data);
                    stream.dirty = true;
                }
                if let Some(update) = take_update(&stream, false) {
                    on_update(update);
                }
            })
        };
        let started = Instant::now();
        let options = ShellExecOptions { on_data, cancel, timeout: args.timeout, env: Some(spawn.env) };
        let exec = self.operations.exec(&spawn.command, &spawn.cwd, options);
        tokio::pin!(exec);
        let mut ticker = tokio::time::interval(UPDATE_THROTTLE);
        let result = loop {
            tokio::select! {
                result = &mut exec => break result,
                _ = ticker.tick() => {
                    if let Some(update) = take_update(&stream, false) {
                        on_update(update);
                    }
                }
            }
        };

        let mut stream = finish(&stream, &on_update);
        let snap = stream.acc.snapshot(true);
        stream.acc.close_temp_file();
        let exit_code = match result {
            Ok(code) => code,
            Err(error) => {
                let (text, _) = format_output(&stream.acc, &snap, "");
                return Err(match error {
                    ShellExecError::Aborted => anyhow!(append_status(&text, "Command aborted")),
                    ShellExecError::TimedOut(secs) => {
                        anyhow!(append_status(&text, &format!("Command timed out after {secs} seconds")))
                    }
                    ShellExecError::Failed(error) => error,
                });
            }
        };
        let (text, details) = format_output(&stream.acc, &snap, "(no output)");
        let Some(code) = exit_code else {
            return Err(anyhow!(append_status(&text, "Command terminated without an exit code")));
        };
        let wall_time = (started.elapsed().as_millis() as f64 / 100.0).round() / 10.0;
        let full = stream.acc.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
        let mut structured = json!({ "output": full.content, "truncated": full.truncated });
        if let (true, Some(path)) = (full.truncated, &snap.full_output_path) {
            structured["full_output_path"] = json!(path.display().to_string());
        }
        structured["exit_code"] = json!(code);
        structured["wall_time_seconds"] = js_number(wall_time);
        let (text, is_error) = if code == 0 {
            (text, false)
        } else {
            (append_status(&text, &format!("Command exited with code {code}")), true)
        };
        Ok(AgentToolResult {
            content: vec![ContentBlock::text(text)],
            details,
            structured_content: Some(structured),
            is_error,
            ..Default::default()
        })
    }
}

/// Stop taking output, finish decoding, and emit the last pending update.
fn finish<'a>(stream: &'a Mutex<Stream>, on_update: &ToolUpdateCallback) -> std::sync::MutexGuard<'a, Stream> {
    {
        let mut guard = stream.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.accepting = false;
        guard.acc.finish();
    }
    if let Some(update) = take_update(stream, true) {
        on_update(update);
    }
    stream.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
