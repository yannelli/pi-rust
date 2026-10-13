//! Shell access for extensions: JavaScript `BashOperations` as Rust `ShellOperations`, the
//! built-in tools extensions create (`createBashTool`, `createReadTool`, ...), local shell
//! commands, and `user_bash` results from custom operations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use pi_agent::{AgentToolResult, ToolUpdateCallback};
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::ops::{Args, error};
use super::ops_async::js_update_callback;
use super::runner::ExtensionRunner;
use crate::agent_session::{BashResult, lock};
use crate::tools::{
    BashToolOptions, LocalShellOperations, ShellDataCallback, ShellExecError, ShellExecOptions, ShellOperations,
    ShellSpawnContext, ToolsOptions,
};

/// Forward output chunks to the JavaScript callback `key`, in order.
fn data_forwarder(runner: &Arc<ExtensionRunner>, key: u64) -> ShellDataCallback {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    let host = runner.host().clone();
    tokio::spawn(async move {
        while let Some(chunk) = receiver.recv().await {
            let _ = host.call_method("invokeCallback", vec![json!(key).into(), json!(chunk).into()]).await;
        }
    });
    Arc::new(move |bytes: &[u8]| {
        let _ = sender.send(String::from_utf8_lossy(bytes).into_owned());
    })
}

/// A JavaScript `BashOperations.exec`, registered as the callback `key`.
pub(crate) struct JsShellOperations {
    runner: Weak<ExtensionRunner>,
    key: u64,
}

impl JsShellOperations {
    pub(crate) fn new(runner: &Arc<ExtensionRunner>, key: u64) -> Self {
        Self { runner: Arc::downgrade(runner), key }
    }
}

#[async_trait]
impl ShellOperations for JsShellOperations {
    async fn exec(&self, command: &str, cwd: &Path, options: ShellExecOptions) -> Result<Option<i32>, ShellExecError> {
        let runner = self
            .runner
            .upgrade()
            .ok_or_else(|| ShellExecError::Failed(anyhow::anyhow!("The extension host has shut down")))?;
        let data_key = runner.next_id();
        let on_data = options.on_data.clone();
        lock(&runner.data).insert(data_key, Arc::new(move |text: &str| on_data(text.as_bytes())));
        let signal = runner.signal_for(&options.cancel);
        let exec_options = json!({
            "dataKey": data_key,
            "signal": signal,
            "timeout": options.timeout,
            "env": options.env,
        });
        let call = vec![
            json!(self.key).into(),
            json!(command).into(),
            json!(cwd.display().to_string()).into(),
            exec_options.into(),
        ];
        let result = runner.host().call_method("invokeCallback", call).await;
        lock(&runner.data).remove(&data_key);
        runner.release_signal(signal);
        if options.cancel.is_cancelled() {
            return Err(ShellExecError::Aborted);
        }
        let result = result.map_err(|failure| ShellExecError::Failed(anyhow::anyhow!(failure.message)))?;
        Ok(result.into_json()["exitCode"].as_i64().map(|code| code as i32))
    }
}

/// A spawn hook calling the JavaScript callback `key` with `{ command, cwd, env }`.
fn spawn_hook(runner: &Arc<ExtensionRunner>, key: u64) -> crate::tools::ShellSpawnHook {
    let weak = Arc::downgrade(runner);
    Arc::new(move |context: ShellSpawnContext| {
        let Some(runner) = weak.upgrade() else { return context };
        let value = json!({"command": context.command, "cwd": context.cwd.display().to_string(), "env": context.env});
        let call = vec![json!(key).into(), value.into()];
        let result = runner.host().blocking_call_method_within("invokeCallback", call, Duration::from_secs(5));
        let Ok(HostValue::Json(changed)) = result else { return context };
        ShellSpawnContext {
            command: changed["command"].as_str().map_or(context.command.clone(), str::to_string),
            cwd: changed["cwd"].as_str().map_or(context.cwd.clone(), PathBuf::from),
            env: serde_json::from_value(changed["env"].clone()).unwrap_or(context.env),
        }
    })
}

/// `tools.builtinExecute`: run a built-in tool for an extension.
pub(crate) async fn builtin_execute(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let (name, cwd, call_id, params, options) = (args.str(0)?, args.str(1)?, args.str(2)?, args.json(3), args.json(4));
    let mut tools_options = ToolsOptions::default();
    if let Some(session) = runner.session() {
        tools_options = ToolsOptions::from_settings(session.settings());
    }
    let mut bash = BashToolOptions { ..tools_options.bash.clone() };
    if let Some(prefix) = options["commandPrefix"].as_str() {
        bash.command_prefix = Some(prefix.to_string());
    }
    if let Some(path) = options["shellPath"].as_str() {
        bash.shell_path = Some(PathBuf::from(path));
    }
    if let Some(key) = options["operations"].as_u64() {
        let operations: Arc<dyn ShellOperations> = Arc::new(JsShellOperations::new(runner, key));
        bash.operations = Some(operations.clone());
        tools_options.powershell.operations = Some(operations);
    }
    if let Some(key) = options["spawnHook"].as_u64() {
        bash.spawn_hook = Some(spawn_hook(runner, key));
        tools_options.powershell.spawn_hook = bash.spawn_hook.clone();
    }
    tools_options.bash = bash;
    let tool = crate::tools::create_tool_with_options(&name, Path::new(&cwd), &tools_options)
        .ok_or_else(|| error(format!("Unknown tool: {name}")))?;
    let cancel = runner.token(options["signal"].as_u64());
    let on_update: ToolUpdateCallback =
        js_update_callback(runner, options["updateKey"].as_u64()).unwrap_or_else(|| Arc::new(|_: AgentToolResult| {}));
    let params = tool.prepare_arguments(params).map_err(|failure| error(format!("{failure:#}")))?;
    let result =
        tool.execute(&call_id, params, cancel, on_update).await.map_err(|failure| error(format!("{failure:#}")))?;
    Ok(json!(result))
}

/// `shell.exec`: a command in the local shell (`createLocalBashOperations().exec`).
pub(crate) async fn exec(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let (shell, command, cwd, options) = (args.str(0)?, args.str(1)?, args.str(2)?, args.json(3));
    let operations: Arc<dyn ShellOperations> = match shell.as_str() {
        "powershell" => Arc::new(crate::tools::LocalPowerShellOperations::default()),
        _ => Arc::new(LocalShellOperations::bash(options["shellPath"].as_str().map(PathBuf::from))),
    };
    let on_data = match options["dataKey"].as_u64() {
        Some(key) => data_forwarder(runner, key),
        None => Arc::new(|_: &[u8]| {}),
    };
    let exec_options = ShellExecOptions {
        on_data,
        cancel: runner.token(options["signal"].as_u64()),
        timeout: options["timeout"].as_f64(),
        env: serde_json::from_value(options["env"].clone()).ok(),
    };
    match operations.exec(&command, Path::new(&cwd), exec_options).await {
        Ok(code) => Ok(json!({"exitCode": code})),
        Err(ShellExecError::Aborted) => Err(error("aborted")),
        Err(ShellExecError::TimedOut(seconds)) => Err(error(format!("timeout:{seconds}"))),
        Err(ShellExecError::Failed(failure)) => Err(error(format!("{failure:#}"))),
    }
}

/// Run a user command (`!cmd`) through custom operations from a `user_bash` handler.
pub(crate) async fn run_user_bash(
    runner: &Arc<ExtensionRunner>,
    key: u64,
    command: &str,
    cwd: &Path,
    cancel: CancellationToken,
    on_chunk: &(dyn Fn(&str) + Send + Sync),
) -> BashResult {
    let output = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = output.clone();
    let options = ShellExecOptions {
        on_data: Arc::new(move |bytes: &[u8]| lock(&sink).push_str(&String::from_utf8_lossy(bytes))),
        cancel: cancel.clone(),
        timeout: None,
        env: None::<BTreeMap<String, String>>,
    };
    let result = JsShellOperations::new(runner, key).exec(command, cwd, options).await;
    let full =
        crate::agent_session::bash::sanitize_binary_output(&crate::agent_session::bash::strip_ansi(&lock(&output)));
    on_chunk(&full);
    let truncation = crate::tools::truncate::truncate_tail(&full, Default::default());
    let full_output_path = truncation.truncated.then(|| {
        let path = std::env::temp_dir().join(format!("pi-bash-{}.log", runner.next_id()));
        std::fs::write(&path, &full).ok().map(|_| path)
    });
    let (exit_code, cancelled) = match result {
        Ok(code) => (code, false),
        Err(ShellExecError::Aborted) => (None, true),
        Err(_) => (Some(1), false),
    };
    BashResult {
        output: if truncation.truncated { truncation.content } else { full },
        exit_code,
        cancelled,
        truncated: truncation.truncated,
        full_output_path: full_output_path.flatten(),
    }
}
