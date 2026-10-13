//! The `powershell` and `bash` cases of upstream `test/tools.test.ts`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_chord::{AbortController, BACKGROUND_CONTEXT, with_abort_signal};
use pi_durable::env::*;
use pi_durable::harness::{DiagnosticSeverity, ToolDiagnostic};
use pi_durable::tools::{
    BashExecution, BashToolOptions, PowerShellToolOptions, ToolCallApi, execute_bash, execute_powershell,
};
use pi_durable::truncate::DEFAULT_MAX_LINES;
use serde_json::json;

use super::support::*;

const TRUNCATED_OUTPUT_LINES: usize = DEFAULT_MAX_LINES + 1;

fn temp_hooked() -> (tempfile::TempDir, HookedEnv) {
    let dir = tempfile::tempdir().unwrap();
    let env = HookedEnv::new(dir.path().to_str().unwrap());
    (dir, env)
}

fn local() -> (tempfile::TempDir, SharedEnv) {
    let dir = tempfile::tempdir().unwrap();
    let env = LocalExecutionEnv::new(LocalEnvOptions::new(dir.path().to_str().unwrap()));
    (dir, Arc::new(env))
}

fn stdout(skipped: Option<ShellOutputSkip>) -> ShellOutputInfo {
    ShellOutputInfo { stream: OutputStream::Stdout, skipped }
}

/// An environment where only the listed programs exist; it records each command and prints `output`.
fn programs_env(installed: &'static [&'static str], output: &'static str, exit_code: i32) -> ProgramsEnv {
    let (dir, mut env) = temp_hooked();
    let commands: Arc<Mutex<Vec<ShellCommand>>> = Arc::default();
    let recorded = commands.clone();
    env.on_exec = Some(Arc::new(move |_, command, options, context| {
        recorded.lock().unwrap().push(command.clone());
        Box::pin(async move {
            let program = match &command {
                ShellCommand::Argv(argv) if installed.contains(&argv[0].as_str()) => None,
                ShellCommand::Argv(argv) => Some(argv[0].clone()),
                ShellCommand::Script(script) => Some(script.chars().take(1).collect()),
            };
            if let Some(program) = program {
                return Err(ExecutionError::new(ExecutionErrorCode::SpawnError, format!("spawn {program} ENOENT")));
            }
            (options.on_output.unwrap())(output, &context, &stdout(None));
            Ok(ShellExecResult { exit_code, spill_path: None })
        })
    }));
    ProgramsEnv { _dir: dir, env: Arc::new(env), commands }
}

struct ProgramsEnv {
    _dir: tempfile::TempDir,
    env: SharedEnv,
    commands: Arc<Mutex<Vec<ShellCommand>>>,
}

impl ProgramsEnv {
    fn argv(&self, index: usize) -> Vec<String> {
        match &self.commands.lock().unwrap()[index] {
            ShellCommand::Argv(argv) => argv.clone(),
            ShellCommand::Script(script) => vec![script.clone()],
        }
    }
}

#[tokio::test]
async fn runs_the_command_with_pwsh_as_one_argument_forcing_utf8_output() {
    let programs = programs_env(&["pwsh"], "héllo\n", 0);
    let api = FakeApi::new(Some(programs.env.clone()));
    let call = args(json!({ "command": "Write-Output 'héllo'" }));
    execute_powershell(&PowerShellToolOptions::default(), call, api.clone(), ctx()).await.unwrap();
    assert_eq!(api.streamed(), "héllo\n");
    assert_eq!(programs.commands.lock().unwrap().len(), 1);
    let script = "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\nWrite-Output 'héllo'";
    let expected = ["pwsh", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script];
    assert_eq!(programs.argv(0), expected);
}

#[tokio::test]
async fn falls_back_to_windows_powershell_and_reports_the_last_start_failure() {
    let windows_only = programs_env(&["powershell"], "ok", 0);
    let api = FakeApi::new(Some(windows_only.env.clone()));
    let options = PowerShellToolOptions { command_prefix: Some("$x = 1".into()), ..Default::default() };
    execute_powershell(&options, args(json!({ "command": "$x" })), api.clone(), ctx()).await.unwrap();
    assert_eq!(api.streamed(), "ok");
    assert_eq!([windows_only.argv(0)[0].as_str(), windows_only.argv(1)[0].as_str()], ["pwsh", "powershell"]);
    assert!(windows_only.argv(1).last().unwrap().ends_with("\n$x = 1\n$x"));

    let none = programs_env(&[], "", 0);
    let call = args(json!({ "command": "1" }));
    let error = execute_powershell(&PowerShellToolOptions::default(), call, FakeApi::new(Some(none.env)), ctx()).await;
    assert_eq!(error.unwrap_err().to_string(), "spawn powershell ENOENT");
}

#[tokio::test]
async fn answers_a_nonzero_exit_with_an_error_result_carrying_the_exit_code() {
    let programs = programs_env(&["pwsh"], "partial", 3);
    let api = FakeApi::new(Some(programs.env.clone()));
    let call = args(json!({ "command": "exit 3" }));
    let result = execute_powershell(&PowerShellToolOptions::default(), call, api.clone(), ctx()).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let structured = serde_json::Value::from(result.structured_output.unwrap());
    assert_eq!(structured, json!({ "output": "partial", "truncated": false, "exitCode": 3 }));
    let exited = ToolDiagnostic::new(DiagnosticSeverity::Error, "Command exited with code 3").with_code("exit_code");
    assert_eq!(result.diagnostics, Some(vec![exited]));
    assert_eq!(api.streamed(), "partial");
}

// "runs real PowerShell with UTF-8 output" runs only on Windows upstream; it is not ported, as no Windows host runs
// these tests.

#[tokio::test]
async fn passes_the_retained_window_to_the_environment_and_forwards_what_it_skipped() {
    let window = ShellOutputWindow { max_bytes: 4, max_lines: 1, min_interval_ms: 100.0, bytes_per_second: 1024.0 };
    let skipped = ShellOutputSkip { bytes: 6, newlines: 2, ends_with_newline: true };
    let received = Arc::new(Mutex::new(None));
    let (_dir, mut env) = temp_hooked();
    let seen = received.clone();
    env.on_exec = Some(Arc::new(move |_, _, options, context| {
        *seen.lock().unwrap() = options.window;
        (options.on_output.unwrap())("tail\n", &context, &stdout(Some(skipped)));
        Box::pin(async { Ok(ShellExecResult { exit_code: 0, spill_path: None }) })
    }));
    let api = Arc::new(FakeApi { env: Some(Arc::new(env)), window: Some(window), ..Default::default() });
    execute_bash(&BashToolOptions::default(), args(json!({ "command": "anything" })), api.clone(), ctx())
        .await
        .unwrap();
    assert_eq!(*received.lock().unwrap(), Some(window));
    assert_eq!(*api.output.lock().unwrap(), [("tail\n".to_string(), Some(skipped))]);
}

#[tokio::test]
async fn streams_combined_stdout_and_stderr_returns_no_output_of_its_own_and_repeats_it_with_the_exit_code() {
    let (_dir, env) = local();
    let api = FakeApi::new(Some(env));
    let call = args(json!({ "command": "printf out; printf err >&2" }));
    let result = execute_bash(&BashToolOptions::default(), call, api.clone(), ctx()).await.unwrap();
    let streamed = api.streamed();
    assert!(streamed.contains("out") && streamed.contains("err"));
    assert_eq!(result.output, None);
    let structured = serde_json::Value::from(result.structured_output.unwrap());
    assert_eq!(structured, json!({ "output": streamed, "truncated": false, "exitCode": 0 }));
}

#[tokio::test]
async fn answers_a_nonzero_exit_with_an_error_result_and_throws_on_a_timeout_after_streaming_the_output() {
    let (_dir, env) = local();
    let api = FakeApi::new(Some(env.clone()));
    let call = args(json!({ "command": "printf failed; exit 7" }));
    let failed = execute_bash(&BashToolOptions::default(), call, api.clone(), ctx()).await.unwrap();
    assert_eq!(failed.is_error, Some(true));
    let structured = serde_json::Value::from(failed.structured_output.unwrap());
    assert_eq!(structured, json!({ "output": "failed", "truncated": false, "exitCode": 7 }));
    let diagnostic = &failed.diagnostics.unwrap()[0];
    assert_eq!(
        (diagnostic.code.as_deref(), diagnostic.message.as_str()),
        (Some("exit_code"), "Command exited with code 7")
    );
    assert_eq!(api.streamed(), "failed");
    let call = args(json!({ "command": "sleep 2", "timeout": 0.01 }));
    let slow = execute_bash(&BashToolOptions::default(), call, FakeApi::new(Some(env)), ctx()).await;
    assert_eq!(slow.unwrap_err().to_string(), "Command timed out after 0.01 seconds");
}

#[tokio::test]
async fn reports_the_spill_of_a_command_that_times_out() {
    let (_dir, mut env) = temp_hooked();
    env.on_exec = Some(Arc::new(|inner, _, options, context| {
        Box::pin(async move {
            let lines: Vec<String> = (1..=TRUNCATED_OUTPUT_LINES).map(|index| format!("line-{index}")).collect();
            let output = format!("{}\n", lines.join("\n"));
            let temp = TempFileOptions { prefix: Some("timeout-".into()), suffix: Some(".log".into()) };
            let spill = inner.create_temp_file(temp, &context).await.unwrap();
            inner.write_file(&spill, output.as_bytes(), &context).await.unwrap();
            (options.on_output.unwrap())(&output, &context, &stdout(None));
            let timeout = options.timeout.unwrap();
            let mut error = ExecutionError::new(ExecutionErrorCode::Timeout, format!("timeout:{timeout}"));
            error.spill_path = Some(spill);
            Err(error)
        })
    }));
    let env: SharedEnv = Arc::new(env);
    let api = FakeApi::new(Some(env.clone()));
    let call = args(json!({ "command": "emit-output-then-time-out", "timeout": 0.05 }));
    let error = execute_bash(&BashToolOptions::default(), call, api.clone(), ctx()).await.unwrap_err();
    assert_eq!(error.to_string(), "Command timed out after 0.05 seconds");
    let reported = api.reported();
    let path = reported[0].message.strip_prefix("Full output: ").expect("a spill diagnostic");
    let full = env.read_text_file(path, &ctx()).await.unwrap();
    assert!(full.contains("line-1\nline-2"));
    assert!(full.contains(&format!("line-{DEFAULT_MAX_LINES}\nline-{TRUNCATED_OUTPUT_LINES}")));
}

#[tokio::test]
async fn prepares_command_cwd_and_an_explicit_environment_with_the_calls_api() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = LocalEnvOptions::new(dir.path().to_str().unwrap());
    options.shell_env = Some(BTreeMap::from([("PI_BASH_PREPARE_INHERITED".into(), "inherited".into())]));
    let env: SharedEnv = Arc::new(LocalExecutionEnv::new(options));
    env.create_dir("workspace", false, &ctx()).await.unwrap();
    let workspace = format!("{}/workspace", env.cwd());
    let received: Arc<Mutex<Option<(SharedEnv, bool)>>> = Arc::default();
    let (seen, target) = (received.clone(), workspace.clone());
    let prepare = Arc::new(
        move |mut execution: BashExecution, api: Arc<dyn ToolCallApi>, context: pi_chord::Context| {
            *seen.lock().unwrap() = Some((api.env().unwrap(), context.abort_signal().is_some()));
            execution.cwd = target.clone();
            execution.env = BTreeMap::from([("PI_BASH_PREPARE_EXPLICIT".into(), "explicit".into())]);
            execution.inherit_env = false;
            execution.command += "\n: > prepared-cwd\nprintf '%s:%s:%s' \"$prefix\" \"${PI_BASH_PREPARE_INHERITED-}\" \"$PI_BASH_PREPARE_EXPLICIT\"";
            // Git Bash on Windows reports $PWD as an MSYS path, so only POSIX compares it.
            if cfg!(unix) {
                execution.command += "\nprintf ':%s' \"$PWD\"";
            }
            Box::pin(async move { Ok(execution) }) as futures::future::BoxFuture<'static, _>
        },
    );
    let options = BashToolOptions { command_prefix: Some("prefix=ready".into()), prepare: Some(prepare) };
    let controller = AbortController::new();
    let context = with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT);
    let api = FakeApi::new(Some(env.clone()));
    execute_bash(&options, args(json!({ "command": ":" })), api.clone(), context).await.unwrap();
    let (prepared_env, has_signal) = received.lock().unwrap().take().unwrap();
    // Upstream compares the signal itself; a Rust AbortSignal has no identity, so the call's context must carry one.
    assert!(Arc::ptr_eq(&prepared_env, &env) && has_signal);
    let pwd =
        if cfg!(unix) { format!(":{}", env.canonical_path(&workspace, &ctx()).await.unwrap()) } else { String::new() };
    assert_eq!(api.streamed(), format!("ready::explicit{pwd}"));
    assert!(env.exists(&format!("{workspace}/prepared-cwd"), &ctx()).await.unwrap());
}

#[tokio::test]
async fn supports_command_prefixes() {
    let (_dir, env) = local();
    let api = FakeApi::new(Some(env));
    let options = BashToolOptions { command_prefix: Some("value=hello".into()), prepare: None };
    execute_bash(&options, args(json!({ "command": "printf $value" })), api.clone(), ctx()).await.unwrap();
    assert_eq!(api.streamed(), "hello");
}

#[tokio::test]
async fn streams_every_byte_and_spills_complete_output_beyond_the_default_limits() {
    let (_dir, env) = local();
    let api = FakeApi::new(Some(env.clone()));
    let call = args(json!({ "command": "i=1; while [ $i -le 3000 ]; do echo line-$i; i=$((i + 1)); done" }));
    execute_bash(&BashToolOptions::default(), call, api.clone(), ctx()).await.unwrap();
    let expected: String = (1..=3000).map(|index| format!("line-{index}\n")).collect();
    assert_eq!(api.streamed(), expected);
    let reported = api.reported();
    let path = reported[0].message.strip_prefix("Full output: ").expect("a spill diagnostic");
    assert_eq!(env.read_text_file(path, &ctx()).await.unwrap(), expected);
}

#[tokio::test]
async fn does_not_spill_output_within_the_limits() {
    let (_dir, env) = local();
    let api = FakeApi::new(Some(env));
    execute_bash(&BashToolOptions::default(), args(json!({ "command": "printf small" })), api.clone(), ctx())
        .await
        .unwrap();
    assert!(api.reported().is_empty());
}
