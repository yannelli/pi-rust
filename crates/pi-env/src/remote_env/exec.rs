//! `Shell::exec` over the daemon's `exec`: timeout validation, environment merge, output events, result precedence.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::{AbortController, Context};
use pi_durable::env::{
    ExecutionError, ExecutionErrorCode, OutputStream, Shell, ShellCommand, ShellExecOptions, ShellExecResult,
    ShellOutputInfo, ShellOutputSkip,
};
use serde_json::{Value, json};

use super::{RemoteExecutionEnv, object};
use crate::connection::{RequestOptions, lock};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;

fn error(code: ExecutionErrorCode, message: impl Into<String>) -> ExecutionError {
    ExecutionError::new(code, message)
}

#[derive(Default)]
struct ExecState {
    settled: bool,
    callback_error: Option<ExecutionError>,
}

/// Removes the command from the running set and the context listener once the command settles.
struct Settle<'a> {
    env: &'a RemoteExecutionEnv,
    state: Arc<Mutex<ExecState>>,
    running: Arc<Mutex<Option<u32>>>,
    listener: Option<(pi_chord::AbortSignal, u64)>,
}

impl Drop for Settle<'_> {
    fn drop(&mut self) {
        lock(&self.state).settled = true;
        if let Some((signal, listener)) = self.listener.take() {
            signal.remove_listener(listener);
        }
        if let Some(id) = lock(&self.running).take() {
            lock(&self.env.running).remove(&id);
        }
    }
}

#[async_trait]
impl Shell for RemoteExecutionEnv {
    async fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        if context.is_cancelled() {
            return Err(error(ExecutionErrorCode::Aborted, "aborted"));
        }
        if let Some(timeout) = options.timeout {
            if !timeout.is_finite() || timeout <= 0.0 {
                let message = "Invalid timeout: must be a finite number of seconds";
                return Err(error(ExecutionErrorCode::Timeout, message));
            }
            if timeout * 1000.0 > MAX_TIMEOUT_MS {
                let message = format!("Invalid timeout: maximum is {} seconds", MAX_TIMEOUT_MS / 1000.0);
                return Err(error(ExecutionErrorCode::Timeout, message));
            }
        }
        let timeout = options.timeout;
        let inherit_env = options.inherit_env.unwrap_or(true);
        let cwd = match options.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => {
                self.resolve(cwd).await.map_err(|failure| error(ExecutionErrorCode::Unknown, failure.message))?
            }
            None => self.cwd.clone(),
        };
        if context.is_cancelled() {
            return Err(error(ExecutionErrorCode::Aborted, "aborted"));
        }
        let mut env = BTreeMap::new();
        if inherit_env {
            env.extend(self.shell_env.clone().unwrap_or_default());
        }
        env.extend(options.env.clone().unwrap_or_default());
        let mut request = match &command {
            ShellCommand::Script(script) => object(&[("command", script.clone().into())]),
            ShellCommand::Argv(argv) => object(&[("argv", json!(argv))]),
        };
        request.insert("cwd".into(), cwd.into());
        request.insert("env".into(), json!(env));
        request.insert("inheritEnv".into(), inherit_env.into());
        if let Some(shell_path) = &self.shell_path {
            request.insert("shellPath".into(), shell_path.clone().into());
        }
        if let Some(timeout) = options.timeout {
            request.insert("timeoutMs".into(), json!(timeout * 1000.0));
        }
        if let Some(spill) = options.spill {
            request.insert("spill".into(), json!({ "afterBytes": spill.after_bytes, "afterLines": spill.after_lines }));
        }
        if let Some(window) = options.window {
            let window = json!({
                "maxBytes": window.max_bytes,
                "maxLines": window.max_lines,
                "minIntervalMs": window.min_interval_ms,
                "bytesPerSecond": window.bytes_per_second,
            });
            request.insert("window".into(), window);
        }

        let controller = AbortController::new();
        let state = Arc::new(Mutex::new(ExecState::default()));
        let running = Arc::new(Mutex::new(None));
        let mut settle = Settle { env: self, state: state.clone(), running: running.clone(), listener: None };
        if let Some(signal) = context.abort_signal() {
            let linked = controller.clone();
            let listener = signal.add_listener(move |_| linked.abort(None));
            settle.listener = Some((signal, listener));
        }
        let (event_state, event_controller, event_context) = (state.clone(), controller.clone(), context.clone());
        let on_output = options.on_output.clone();
        let on_event = Arc::new(move |event: &serde_json::Map<String, Value>, payload: &[u8]| {
            if event.get("kind").and_then(Value::as_str) != Some("output") {
                return;
            }
            {
                let state = lock(&event_state);
                if state.settled || state.callback_error.is_some() {
                    return;
                }
            }
            let text = String::from_utf8_lossy(payload);
            let Some(on_output) = &on_output else { return };
            if text.is_empty() {
                return;
            }
            let stream = match event.get("stream").and_then(Value::as_str) {
                Some("stderr") => OutputStream::Stderr,
                _ => OutputStream::Stdout,
            };
            let skipped = event
                .get("skipped")
                .and_then(|skipped| serde_json::from_value::<ShellOutputSkip>(skipped.clone()).ok());
            let info = ShellOutputInfo { stream, skipped };
            if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| on_output(&text, &event_context, &info))) {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
                    .unwrap_or_else(|| "Output callback failed".into());
                lock(&event_state).callback_error = Some(error(ExecutionErrorCode::CallbackError, message));
                event_controller.abort(None);
            }
        });
        let started = self.running.clone();
        let on_start = Box::new(move |id: u32, session: u64| {
            *lock(&running) = Some(id);
            lock(&started).insert(id, session);
        });
        let request_options = RequestOptions {
            signal: Some(controller.signal()),
            on_event: Some(on_event),
            on_start: Some(on_start),
            ..Default::default()
        };
        let reply = self.connection.request("exec", request, request_options).await;
        let callback_error = lock(&state).callback_error.take();
        drop(settle);
        if let Some(callback_error) = callback_error {
            return Err(callback_error);
        }
        match reply {
            Ok(reply) => Ok(ShellExecResult {
                exit_code: reply.json.get("exitCode").and_then(Value::as_i64).unwrap_or(0) as i32,
                spill_path: reply.json.get("spillPath").and_then(Value::as_str).map(str::to_string),
            }),
            Err(remote) => {
                let mut failure = match remote.code.as_str() {
                    "timeout" => {
                        let timeout = timeout.map_or("undefined".into(), |timeout| timeout.to_string());
                        error(ExecutionErrorCode::Timeout, format!("timeout:{timeout}"))
                    }
                    "aborted" => error(ExecutionErrorCode::Aborted, "aborted"),
                    "shell_unavailable" => error(ExecutionErrorCode::ShellUnavailable, remote.message.clone()),
                    "spawn_error" => error(ExecutionErrorCode::SpawnError, remote.message.clone()),
                    _ => error(ExecutionErrorCode::Unknown, remote.message.clone()),
                };
                failure.spill_path = remote.fields.get("spillPath").and_then(Value::as_str).map(str::to_string);
                Err(failure)
            }
        }
    }
}
