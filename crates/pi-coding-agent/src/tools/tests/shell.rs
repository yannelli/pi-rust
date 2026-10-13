//! Shell tool tests ported from upstream `tools.test.ts`, `powershell-tool.test.ts`, and the
//! spawn hook test of `agent-session-dynamic-tools.test.ts`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{noop, text};
use crate::tools::{
    BashToolOptions, POWERSHELL_ARGS, PowerShellToolOptions, ShellExecError, ShellExecOptions, ShellOperations,
    ShellSpawnContext, ShellTool, create_bash_tool, create_powershell_tool,
};

async fn bash(tool: &dyn AgentTool, command: &str) -> anyhow::Result<AgentToolResult> {
    tool.execute("call-1", json!({ "command": command }), CancellationToken::new(), noop()).await
}

type Script = Arc<dyn Fn(&ShellExecOptions) -> Result<Option<i32>, ShellExecError> + Send + Sync>;

/// Operations that record the command and play a script instead of running a process.
struct FakeOperations {
    commands: Mutex<Vec<String>>,
    script: Script,
}

impl FakeOperations {
    fn new(
        script: impl Fn(&ShellExecOptions) -> Result<Option<i32>, ShellExecError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self { commands: Mutex::new(Vec::new()), script: Arc::new(script) })
    }
}

#[async_trait]
impl ShellOperations for FakeOperations {
    async fn exec(&self, command: &str, _cwd: &Path, options: ShellExecOptions) -> Result<Option<i32>, ShellExecError> {
        self.commands.lock().unwrap().push(command.to_string());
        (self.script)(&options)
    }
}

#[cfg(unix)]
#[tokio::test]
async fn reports_exit_codes_and_output_as_structured_content() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_bash_tool(dir.path(), BashToolOptions::default());
    let failed = bash(tool.as_ref(), "echo out; exit 3").await.unwrap();
    assert!(failed.is_error);
    assert_eq!(text(&failed), "out\n\n\nCommand exited with code 3");
    let structured = failed.structured_content.unwrap();
    assert_eq!(structured["output"], "out\n");
    assert_eq!(structured["truncated"], false);
    assert_eq!(structured["exit_code"], 3);
    assert!(structured["wall_time_seconds"].is_number() && structured.get("full_output_path").is_none());

    let ok = bash(tool.as_ref(), "echo fine").await.unwrap();
    assert!(!ok.is_error);
    assert_eq!(ok.structured_content.as_ref().unwrap()["output"], "fine\n");
    assert_eq!(ok.structured_content.unwrap()["exit_code"], 0);
    let empty = bash(tool.as_ref(), "true").await.unwrap();
    assert_eq!(text(&empty), "(no output)");
    assert_eq!(empty.structured_content.unwrap()["output"], "");
}

#[cfg(unix)]
#[tokio::test]
async fn returns_up_to_one_mebibyte_of_output_in_structured_content() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_bash_tool(dir.path(), BashToolOptions::default());
    let medium = bash(tool.as_ref(), "seq 1 3000").await.unwrap();
    assert_eq!(medium.details.as_ref().unwrap()["truncation"]["truncated"], true);
    let output = medium.structured_content.unwrap();
    assert_eq!(output["truncated"], false);
    let expected: String = (1..=3000).map(|n| format!("{n}\n")).collect();
    assert_eq!(output["output"], expected);

    let large = bash(tool.as_ref(), "seq 1 300000").await.unwrap();
    let output = large.structured_content.unwrap();
    let content = output["output"].as_str().unwrap();
    assert_eq!(output["truncated"], true);
    assert!(content.starts_with("1\n2\n3\n") && content.ends_with("299999\n300000\n"));
    assert!(regex::Regex::new(r"\n\n\[\.\.\. \d+ bytes omitted \.\.\.\]\n\n").unwrap().is_match(content));
    assert!(content.len() < 1024 * 1024 + 100);
    let path = output["full_output_path"].as_str().unwrap();
    assert_eq!(large.details.unwrap()["fullOutputPath"], path);
    assert!(std::fs::read_to_string(path).unwrap().ends_with("300000\n"));
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
#[tokio::test]
async fn signal_terminations_exit_with_128_plus_the_signal() {
    let dir = tempfile::tempdir().unwrap();
    let operations = crate::tools::LocalShellOperations::bash(None);
    let tool = create_bash_tool(dir.path(), BashToolOptions::default());
    for (signal, code) in [("KILL", 137), ("TERM", 143)] {
        let options =
            ShellExecOptions { on_data: Arc::new(|_| {}), cancel: CancellationToken::new(), timeout: None, env: None };
        let exit = operations.exec(&format!("kill -{signal} $$"), dir.path(), options).await.unwrap();
        assert_eq!(exit, Some(code));
        let result = bash(tool.as_ref(), &format!("printf 'before-kill\\n'; kill -{signal} $$")).await.unwrap();
        assert!(result.is_error);
        assert_eq!(text(&result), format!("before-kill\n\n\nCommand exited with code {code}"));
    }
}

#[tokio::test]
async fn rejects_a_missing_exit_code_from_custom_operations() {
    let operations = FakeOperations::new(|options| {
        (options.on_data)(b"partial\n");
        Ok(None)
    });
    let dir = tempfile::tempdir().unwrap();
    let tool = create_bash_tool(dir.path(), BashToolOptions { operations: Some(operations), ..Default::default() });
    let error = bash(tool.as_ref(), "remote").await.unwrap_err();
    assert_eq!(error.to_string(), "partial\n\n\nCommand terminated without an exit code");
}

#[tokio::test]
async fn timeout_and_abort_errors_name_the_full_output_file() {
    let full_output = regex::Regex::new(r"\[Showing lines \d+-\d+ of \d+\. Full output: ([^\]\n]+)\]").unwrap();
    for (error, expected) in [(0, "Command timed out after 5 seconds"), (1, "Command aborted")] {
        let operations = FakeOperations::new(move |options| {
            for line in 1..=3000 {
                (options.on_data)(format!("{line}\n").as_bytes());
            }
            Err(if error == 0 { ShellExecError::TimedOut(5.0) } else { ShellExecError::Aborted })
        });
        let dir = tempfile::tempdir().unwrap();
        let tool = create_bash_tool(dir.path(), BashToolOptions { operations: Some(operations), ..Default::default() });
        let message = bash(tool.as_ref(), "chatty-fail").await.unwrap_err().to_string();
        assert!(message.ends_with(expected), "{message}");
        let path = full_output.captures(&message).unwrap()[1].to_string();
        let full = std::fs::read_to_string(&path).unwrap();
        assert!(full.contains("1\n2\n3") && full.contains("2998\n2999\n3000"));
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test]
async fn reports_a_missing_working_directory_and_shell() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_bash_tool(&dir.path().join("missing"), BashToolOptions::default());
    let error = bash(tool.as_ref(), "echo test").await.unwrap_err().to_string();
    assert!(
        error.starts_with("Working directory does not exist: ") && error.ends_with("Cannot execute bash commands.")
    );
    let options = BashToolOptions { shell_path: Some("/custom/bash".into()), ..Default::default() };
    let error = bash(create_bash_tool(dir.path(), options).as_ref(), "echo test").await.unwrap_err();
    assert_eq!(error.to_string(), "Custom shell path not found: /custom/bash");
}

#[tokio::test]
async fn custom_operations_take_precedence_over_the_shell_path() {
    let operations = FakeOperations::new(|_| Ok(Some(0)));
    let options = BashToolOptions {
        operations: Some(operations.clone()),
        shell_path: Some("/custom/bash".into()),
        command_prefix: Some("set -e".into()),
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let result = bash(create_bash_tool(dir.path(), options).as_ref(), "echo test").await.unwrap();
    assert_eq!(text(&result), "(no output)");
    assert_eq!(*operations.commands.lock().unwrap(), ["set -e\necho test"]);
}

#[cfg(unix)]
#[tokio::test]
async fn sends_the_command_over_stdin_when_the_shell_requires_it() {
    use crate::tools::{CommandTransport, LocalShellOperations, ShellConfig};
    let cat = crate::tools::shell_process::find_executable_on_path("cat").expect("cat on PATH");
    let operations = LocalShellOperations::new("bash", move || {
        Ok(ShellConfig { shell: cat.clone(), args: Vec::new(), transport: CommandTransport::Stdin })
    });
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let sink = chunks.clone();
    let on_data = Arc::new(move |data: &[u8]| sink.lock().unwrap().extend_from_slice(data));
    let options = ShellExecOptions { on_data, cancel: CancellationToken::new(), timeout: None, env: None };
    let command = "name='World'; echo \"Hello, ${name}!\"";
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(operations.exec(command, dir.path(), options).await.unwrap(), Some(0));
    assert_eq!(String::from_utf8(chunks.lock().unwrap().clone()).unwrap(), command);
}

fn session_variables() -> BTreeMap<String, String> {
    [("PI_SESSION_ID", "session-1"), ("PI_PROVIDER", "anthropic"), ("PI_MODEL", "claude"), ("OTHER", "dropped")]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[tokio::test]
async fn exposes_session_variables_to_spawn_hooks_and_supports_opting_out() {
    let seen: Arc<Mutex<Vec<ShellSpawnContext>>> = Arc::default();
    let sink = seen.clone();
    let hook = Arc::new(move |context: ShellSpawnContext| {
        sink.lock().unwrap().push(context.clone());
        ShellSpawnContext { command: format!("hooked {}", context.command), ..context }
    });
    let operations = FakeOperations::new(|_| Ok(Some(0)));
    let dir = tempfile::tempdir().unwrap();
    let options = |expose: bool| BashToolOptions {
        operations: Some(operations.clone()),
        expose_session_environment: expose,
        spawn_hook: Some(hook.clone()),
        session_environment: Some(Arc::new(session_variables)),
        ..Default::default()
    };
    let exposed = create_bash_tool(dir.path(), options(true));
    assert_eq!(
        exposed.prompt_guidelines(),
        ["You can inspect PI_* environment variables for current model and session details."]
    );
    bash(exposed.as_ref(), "printf ok").await.unwrap();
    let hidden = create_bash_tool(dir.path(), options(false));
    assert!(hidden.prompt_guidelines().is_empty());
    bash(hidden.as_ref(), "printf ok").await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].env.get("PI_SESSION_ID").map(String::as_str), Some("session-1"));
    assert_eq!(seen[0].env.get("PI_MODEL").map(String::as_str), Some("claude"));
    assert_ne!(seen[0].env.get("OTHER").map(String::as_str), Some("dropped"));
    assert_eq!(seen[0].cwd, dir.path());
    for name in ["PI_SESSION_ID", "PI_SESSION_FILE", "PI_PROVIDER", "PI_MODEL", "PI_REASONING_LEVEL"] {
        assert!(!seen[1].env.contains_key(name), "{name}");
    }
    assert_eq!(*operations.commands.lock().unwrap(), ["hooked printf ok", "hooked printf ok"]);
}

#[test]
fn powershell_uses_a_process_local_execution_policy_bypass() {
    assert_eq!(POWERSHELL_ARGS, ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"]);
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::powershell(dir.path().to_path_buf(), PowerShellToolOptions::default());
    assert_eq!((tool.name(), tool.label(), tool.config().prompt), ("powershell", "powershell", "PS>"));
    assert_eq!(tool.prompt_snippet(), Some("Execute PowerShell commands"));
    assert!(tool.description().starts_with("Execute a PowerShell command in the current working directory."));
}

#[tokio::test]
async fn powershell_runs_through_custom_operations_anywhere() {
    let operations = FakeOperations::new(|options| {
        (options.on_data)("héllo €\n".as_bytes());
        Ok(Some(0))
    });
    let dir = tempfile::tempdir().unwrap();
    let options = PowerShellToolOptions { operations: Some(operations.clone()), ..Default::default() };
    let result = bash(create_powershell_tool(dir.path(), options).as_ref(), "Write-Output 'x'").await.unwrap();
    assert_eq!(text(&result), "héllo €\n");
    assert_eq!(*operations.commands.lock().unwrap(), ["Write-Output 'x'"]);
}

#[cfg(not(windows))]
#[tokio::test]
async fn local_powershell_is_windows_only() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_powershell_tool(dir.path(), PowerShellToolOptions::default());
    let error = bash(tool.as_ref(), "Write-Output 'x'").await.unwrap_err();
    assert_eq!(error.to_string(), "The powershell tool is only available on Windows.");
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_executes_commands_with_utf8_output() {
    let config = crate::tools::powershell_shell_config().unwrap();
    assert_eq!(config.args, POWERSHELL_ARGS);
    let dir = tempfile::tempdir().unwrap();
    let tool = create_powershell_tool(dir.path(), PowerShellToolOptions::default());
    let result = bash(tool.as_ref(), "Write-Output 'héllo €'; Get-ExecutionPolicy -Scope Process").await.unwrap();
    let output = text(&result);
    assert!(output.contains("héllo €") && output.contains("Bypass"), "{output}");
}

#[test]
fn shell_output_schema_validates_structured_content() {
    let structured: Value = json!({ "output": "", "truncated": false, "exit_code": 0, "wall_time_seconds": 0.1 });
    assert!(pi_agent::validate_tool_arguments("bash", &crate::tools::shell_output_schema(), &structured).is_ok());
}
