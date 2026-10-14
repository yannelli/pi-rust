//! The `bash` and `powershell` tools: run a command through the environment, streaming output to the call.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::JsonObject;
use pi_chord::Context;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolCallApi, parse_args, require_env, thrown};
use crate::env::{ExecutionErrorCode, ShellCommand, ShellExecOptions, ShellSpillOptions};
use crate::errors::{DurableError, DurableResult};
use crate::harness::{
    DiagnosticSeverity, Retain, ToolDiagnostic, ToolExecutionResult, ToolOutputLimits, ToolRegistration, define_tool,
};
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;

#[derive(Deserialize)]
struct ShellToolInput {
    command: String,
    timeout: Option<f64>,
}

/// A command about to run, which `prepare` may change: the script, its working directory and environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BashExecution {
    pub command: String,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub inherit_env: bool,
}

/// Changes an execution before it runs; it gets the execution and returns the one to run.
pub type BashPrepare = Arc<
    dyn Fn(BashExecution, Arc<dyn ToolCallApi>, Context) -> BoxFuture<'static, DurableResult<BashExecution>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct BashToolOptions {
    /// Lines run before each command.
    pub command_prefix: Option<String>,
    pub prepare: Option<BashPrepare>,
}

#[derive(Clone, Default)]
pub struct PowerShellToolOptions {
    /// Lines run before each command.
    pub command_prefix: Option<String>,
    pub prepare: Option<BashPrepare>,
    /// Programs to try in order; default `pwsh`, then `powershell`. A program that cannot start is skipped.
    pub programs: Option<Vec<String>>,
}

fn shell_schema(what: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": format!("{what} command to execute") },
            "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
        },
        "required": ["command"]
    })
}

/// What a program that calls `bash` or `powershell` receives.
fn shell_structured_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "output": { "type": "string", "description": "Combined stdout and stderr: the retained tail, as the model sees it" },
            "truncated": { "type": "boolean", "description": "Whether earlier output was dropped" },
            "fullOutputPath": { "type": "string", "description": "File with the complete output, when it was spilled" },
            "exitCode": { "type": "number" }
        },
        "required": ["output", "truncated", "exitCode"]
    })
}

fn shell_description(what: &str) -> String {
    format!(
        "Execute a {what} command in the current working directory. Returns combined stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
        DEFAULT_MAX_BYTES / 1024
    )
}

fn validate_timeout(timeout: Option<f64>) -> DurableResult<()> {
    let Some(timeout) = timeout else { return Ok(()) };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(DurableError::error("Invalid timeout: must be a finite number of seconds"));
    }
    if timeout > MAX_TIMEOUT_SECONDS {
        return Err(DurableError::error(format!("Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds")));
    }
    Ok(())
}

/// The execution of one call: the command with its prefix, in the environment's working directory, then `prepare`.
async fn prepare_execution(
    command: String,
    prefix: Option<&str>,
    prepare: Option<&BashPrepare>,
    api: &Arc<dyn ToolCallApi>,
    context: &Context,
) -> DurableResult<BashExecution> {
    let command = match prefix.filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command,
    };
    let execution =
        BashExecution { command, cwd: require_env(api.as_ref())?.cwd(), env: BTreeMap::new(), inherit_env: true };
    match prepare {
        Some(prepare) => prepare(execution, api.clone(), context.clone()).await,
        None => Ok(execution),
    }
}

/// Run each command form in turn until one starts, streaming output to the call within the window it keeps, and turn
/// the outcome into the tool's result: a spill diagnostic, an error result for a nonzero exit, and an error for a
/// failure such as a timeout. The structured output repeats the kept output with the exit code.
async fn run_command(
    commands: Vec<ShellCommand>,
    execution: &BashExecution,
    timeout: Option<f64>,
    api: &Arc<dyn ToolCallApi>,
    context: &Context,
) -> DurableResult<ToolExecutionResult> {
    let env = require_env(api.as_ref())?;
    let mut result = None;
    for command in commands {
        let sink = api.clone();
        let options = ShellExecOptions {
            cwd: Some(execution.cwd.clone()),
            env: Some(execution.env.clone()),
            inherit_env: Some(execution.inherit_env),
            timeout,
            on_output: Some(Arc::new(move |text, _context, info| _ = sink.output(text, info.skipped))),
            spill: Some(ShellSpillOptions {
                after_bytes: DEFAULT_MAX_BYTES as u64,
                after_lines: DEFAULT_MAX_LINES as u64,
            }),
            // An environment may then omit output outside the kept tail and report the omission.
            window: api.output_window(),
        };
        let outcome = env.exec(command, options, context).await;
        // A program that could not start produced no output; the next form may.
        let started = !matches!(&outcome, Err(error) if error.code == ExecutionErrorCode::SpawnError);
        result = Some(outcome);
        if started {
            break;
        }
    }
    let result = result.ok_or_else(|| DurableError::error("No command to run"))?;
    let spill_path = match &result {
        Ok(value) => value.spill_path.clone(),
        Err(error) => error.spill_path.clone(),
    };
    if let Some(path) = &spill_path {
        let message = format!("Full output: {path}");
        api.diagnostic(ToolDiagnostic::new(DiagnosticSeverity::Info, message).with_code("full_output"))?;
    }
    let exit_code = match result {
        Ok(value) => value.exit_code,
        Err(error) if error.code == ExecutionErrorCode::Aborted && context.is_cancelled() => return Err(thrown(error)),
        Err(error) if error.code == ExecutionErrorCode::Timeout => {
            let seconds = timeout.map(|t| t.to_string()).unwrap_or_else(|| "undefined".into());
            return Err(DurableError::error(format!("Command timed out after {seconds} seconds")));
        }
        Err(error) if error.code == ExecutionErrorCode::Aborted => return Err(DurableError::error("Command aborted")),
        Err(error) => return Err(thrown(error)),
    };
    let (output, truncated) = api.retained_output()?;
    let mut structured = json!({ "output": output, "truncated": truncated });
    if let Some(path) = spill_path {
        structured["fullOutputPath"] = json!(path);
    }
    structured["exitCode"] = json!(exit_code);
    let mut result = ToolExecutionResult { structured_output: Some(structured.into()), ..Default::default() };
    if exit_code != 0 {
        let message = format!("Command exited with code {exit_code}");
        result.is_error = Some(true);
        result.diagnostics = Some(vec![ToolDiagnostic::new(DiagnosticSeverity::Error, message).with_code("exit_code")]);
    }
    Ok(result)
}

fn tail_limits() -> ToolOutputLimits {
    ToolOutputLimits { retain: Some(Retain::Tail), ..Default::default() }
}

/// Runs a command through the environment's shell. Its output streams to the call, where the Harness keeps the tail;
/// output beyond the limits is spilled to a file reported as a diagnostic. A nonzero exit is an error result with the
/// exit code; a timeout fails the call, which makes an error result that still carries the output and diagnostics.
pub fn create_bash_tool(options: BashToolOptions) -> ToolRegistration {
    let options = Arc::new(options);
    define_tool("bash", shell_description("bash"), shell_schema("Bash"), move |args, api, context| {
        let options = options.clone();
        async move { execute_bash(&options, args, Arc::new(api), context).await }
    })
    .with_structured_output_schema(shell_structured_output_schema())
    .with_output_limits(tail_limits())
}

/// The `bash` tool's execution against any call api.
pub async fn execute_bash(
    options: &BashToolOptions,
    args: JsonObject,
    api: Arc<dyn ToolCallApi>,
    context: Context,
) -> DurableResult<ToolExecutionResult> {
    let ShellToolInput { command, timeout } = parse_args(args)?;
    validate_timeout(timeout)?;
    let execution =
        prepare_execution(command, options.command_prefix.as_deref(), options.prepare.as_ref(), &api, &context).await?;
    let commands = vec![ShellCommand::Script(execution.command.clone())];
    run_command(commands, &execution, timeout, &api, &context).await
}

/// Output in UTF-8 whatever the console's code page, as the coding agent's `powershell` tool does.
const UTF8_OUTPUT: &str = "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}";
const POWERSHELL_ARGS: [&str; 5] = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"];

/// Runs a PowerShell command like `bash`, through `pwsh` (PowerShell 7) or else Windows PowerShell, started directly
/// with the command as an argument, so no other shell parses it.
pub fn create_powershell_tool(options: PowerShellToolOptions) -> ToolRegistration {
    let options = Arc::new(options);
    define_tool("powershell", shell_description("PowerShell"), shell_schema("PowerShell"), move |args, api, context| {
        let options = options.clone();
        async move { execute_powershell(&options, args, Arc::new(api), context).await }
    })
    .with_structured_output_schema(shell_structured_output_schema())
    .with_output_limits(tail_limits())
}

/// The `powershell` tool's execution against any call api.
pub async fn execute_powershell(
    options: &PowerShellToolOptions,
    args: JsonObject,
    api: Arc<dyn ToolCallApi>,
    context: Context,
) -> DurableResult<ToolExecutionResult> {
    let ShellToolInput { command, timeout } = parse_args(args)?;
    validate_timeout(timeout)?;
    let execution =
        prepare_execution(command, options.command_prefix.as_deref(), options.prepare.as_ref(), &api, &context).await?;
    let script = format!("{UTF8_OUTPUT}\n{}", execution.command);
    let default_programs = ["pwsh".to_string(), "powershell".to_string()];
    let programs = options.programs.as_deref().unwrap_or(&default_programs);
    let commands = programs
        .iter()
        .map(|program| {
            let mut argv = vec![program.clone()];
            argv.extend(POWERSHELL_ARGS.iter().map(|arg| arg.to_string()));
            argv.push(script.clone());
            ShellCommand::Argv(argv)
        })
        .collect();
    run_command(commands, &execution, timeout, &api, &context).await
}
