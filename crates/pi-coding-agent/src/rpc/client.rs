//! A client for `pi --mode rpc` (upstream `modes/rpc/rpc-client.ts`): it starts the process,
//! correlates responses by id, and hands every other record to event subscribers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};

use super::client_types::RpcUiResponse;
use super::jsonl::{JsonlReader, serialize_json_line};
use super::tree::parse_json_line;
use super::types::RpcCommand;
use crate::agent_session::lock;

/// How to start the agent process.
#[derive(Clone, Debug)]
pub struct RpcClientOptions {
    /// The `pi` executable (default: `pi` on `PATH`).
    pub program: PathBuf,
    pub cwd: Option<PathBuf>,
    /// Variables added to the inherited environment.
    pub env: Vec<(String, String)>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Extra arguments after `--mode rpc` and the model flags.
    pub args: Vec<String>,
    /// How long a command may wait for its response (upstream: 30 seconds).
    pub request_timeout: Duration,
    /// Copy the agent's stderr to this process's stderr as upstream does.
    pub forward_stderr: bool,
}

impl Default for RpcClientOptions {
    fn default() -> Self {
        Self {
            program: PathBuf::from("pi"),
            cwd: None,
            env: Vec::new(),
            provider: None,
            model: None,
            args: Vec::new(),
            request_timeout: Duration::from_secs(30),
            forward_stderr: true,
        }
    }
}

type Pending = oneshot::Sender<Result<Value, String>>;

#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<String, Pending>>,
    listeners: Mutex<Vec<mpsc::UnboundedSender<Value>>>,
    stderr: Mutex<String>,
    exit_error: Mutex<Option<String>>,
}

impl Shared {
    fn stderr(&self) -> String {
        lock(&self.stderr).clone()
    }

    fn fail_pending(&self, message: &str) {
        for (_, pending) in lock(&self.pending).drain() {
            let _ = pending.send(Err(message.to_string()));
        }
    }

    fn dispatch(&self, record: Value) {
        if record.get("type").and_then(Value::as_str) == Some("response")
            && let Some(id) = record.get("id").and_then(Value::as_str)
            && let Some(pending) = lock(&self.pending).remove(id)
        {
            let _ = pending.send(Ok(record));
            return;
        }
        lock(&self.listeners).retain(|listener| listener.send(record.clone()).is_ok());
    }
}

struct Process {
    stdin: Option<ChildStdin>,
    kill: Option<oneshot::Sender<()>>,
    exited: watch::Receiver<bool>,
}

/// A running `pi --mode rpc` process with typed commands (see `client_api.rs`).
pub struct RpcClient {
    options: RpcClientOptions,
    shared: Arc<Shared>,
    process: tokio::sync::Mutex<Option<Process>>,
    next_id: AtomicU64,
}

fn signal_name(status: &ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let name = match signal {
                1 => "SIGHUP".to_string(),
                2 => "SIGINT".to_string(),
                9 => "SIGKILL".to_string(),
                15 => "SIGTERM".to_string(),
                other => format!("SIG{other}"),
            };
            return name;
        }
    }
    let _ = status;
    "null".into()
}

fn exit_message(status: Option<ExitStatus>, stderr: &str) -> String {
    let code = status.and_then(|status| status.code()).map_or_else(|| "null".to_string(), |code| code.to_string());
    let signal = status.map_or_else(|| "null".to_string(), |status| signal_name(&status));
    format!("Agent process exited (code={code} signal={signal}). Stderr: {stderr}")
}

async fn read_records(stdout: impl AsyncRead + Unpin, shared: Arc<Shared>) {
    let mut reader = JsonlReader::new(stdout);
    while let Ok(Some(line)) = reader.next_line().await {
        let parsed = match serde_json::from_str::<Value>(&line) {
            Ok(value) => Ok(value),
            Err(error) if error.to_string().starts_with("recursion limit exceeded") => {
                tokio::task::spawn_blocking(move || parse_json_line(&line)).await.unwrap_or(Err(String::new()))
            }
            Err(error) => Err(error.to_string()),
        };
        // Lines that are not JSON are ignored, as upstream does.
        if let Ok(record) = parsed {
            shared.dispatch(record);
        }
    }
}

async fn read_stderr(
    mut stderr: impl AsyncRead + Unpin,
    shared: Arc<Shared>,
    forward: bool,
    done: oneshot::Sender<()>,
) {
    let _done = done;
    let mut buffer = vec![0u8; 8192];
    while let Ok(read) = stderr.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buffer[..read]);
        lock(&shared.stderr).push_str(&text);
        if forward {
            eprint!("{text}");
        }
    }
}

async fn wait_for_exit(
    mut child: Child,
    kill: oneshot::Receiver<()>,
    stderr_done: oneshot::Receiver<()>,
    shared: Arc<Shared>,
    exited: watch::Sender<bool>,
) {
    let status = tokio::select! {
        status = child.wait() => status.ok(),
        _ = kill => {
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };
    // Let the last stderr output into the message; descendants may keep the pipe open.
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr_done).await;
    let message = exit_message(status, &shared.stderr());
    lock(&shared.exit_error).get_or_insert(message.clone());
    shared.fail_pending(&message);
    exited.send_replace(true);
}

impl RpcClient {
    pub fn new(options: RpcClientOptions) -> Self {
        Self { options, shared: Arc::default(), process: tokio::sync::Mutex::new(None), next_id: AtomicU64::new(0) }
    }

    /// Start the agent and give it a moment to fail on bad arguments.
    pub async fn start(&self) -> anyhow::Result<()> {
        let mut process = self.process.lock().await;
        if process.is_some() {
            anyhow::bail!("Client already started");
        }
        *lock(&self.shared.exit_error) = None;
        let mut command = Command::new(&self.options.program);
        command.args(["--mode", "rpc"]);
        if let Some(provider) = &self.options.provider {
            command.args(["--provider", provider]);
        }
        if let Some(model) = &self.options.model {
            command.args(["--model", model]);
        }
        command.args(&self.options.args).envs(self.options.env.iter().map(|(key, value)| (key, value)));
        if let Some(cwd) = &self.options.cwd {
            command.current_dir(cwd);
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| anyhow::anyhow!("Agent process error: {error}. Stderr: {}", self.shared.stderr()))?;
        let stdin = child.stdin.take();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(read_records(stdout, self.shared.clone()));
        }
        let (stderr_done, stderr_closed) = oneshot::channel();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(read_stderr(stderr, self.shared.clone(), self.options.forward_stderr, stderr_done));
        }
        let (kill, kill_receiver) = oneshot::channel();
        let (exited_sender, exited) = watch::channel(false);
        tokio::spawn(wait_for_exit(child, kill_receiver, stderr_closed, self.shared.clone(), exited_sender));
        *process = Some(Process { stdin, kill: Some(kill), exited: exited.clone() });
        drop(process);
        tokio::time::sleep(Duration::from_millis(100)).await;
        if *exited.borrow() {
            anyhow::bail!(self.exit_error().unwrap_or_else(|| exit_message(None, &self.shared.stderr())));
        }
        Ok(())
    }

    /// Close stdin for an orderly shutdown; kill the process if it has not exited after a second.
    pub async fn stop(&self) {
        let Some(mut process) = self.process.lock().await.take() else { return };
        drop(process.stdin.take());
        let mut exited = process.exited.clone();
        let graceful = tokio::time::timeout(Duration::from_secs(1), exited.wait_for(|done| *done)).await.is_ok();
        if !graceful {
            if let Some(kill) = process.kill.take() {
                let _ = kill.send(());
            }
            let _ = exited.wait_for(|done| *done).await;
        }
        lock(&self.shared.pending).clear();
    }

    /// Records that are not responses to this client's commands: session events,
    /// `extension_ui_request`, `extension_error`, and responses without a known id.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Value> {
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.shared.listeners).push(sender);
        receiver
    }

    /// Everything the agent wrote to stderr so far.
    pub fn stderr(&self) -> String {
        self.shared.stderr()
    }

    /// Why the process ended, once it has.
    pub fn exit_error(&self) -> Option<String> {
        lock(&self.shared.exit_error).clone()
    }

    async fn write_line(&self, line: &str) -> anyhow::Result<()> {
        let mut process = self.process.lock().await;
        let process = process.as_mut().ok_or_else(|| anyhow::anyhow!("Client not started"))?;
        if let Some(error) = self.exit_error() {
            anyhow::bail!(error);
        }
        let stdin = process.stdin.as_mut().ok_or_else(|| anyhow::anyhow!("Client not started"))?;
        let written = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        };
        written.await.map_err(|error| {
            let message = self.exit_error().unwrap_or_else(|| {
                format!("Agent process stdin is not writable: {error}. Stderr: {}", self.shared.stderr())
            });
            anyhow::anyhow!(message)
        })
    }

    /// Send a record that gets no response, such as an `extension_ui_response`.
    pub async fn send_record(&self, record: &Value) -> anyhow::Result<()> {
        self.write_line(&serialize_json_line(record)).await
    }

    /// Answer an `extension_ui_request` dialog.
    pub async fn respond_to_ui(&self, id: &str, response: RpcUiResponse) -> anyhow::Result<()> {
        self.send_record(&response.to_record(id)).await
    }

    /// Send `command` with a fresh id and wait for its response record.
    pub async fn send(&self, command: &RpcCommand) -> anyhow::Result<Value> {
        let mut record = serde_json::to_value(command)?;
        let kind = record["type"].as_str().unwrap_or_default().to_string();
        let id = format!("req_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        record["id"] = Value::String(id.clone());
        let (sender, receiver) = oneshot::channel();
        lock(&self.shared.pending).insert(id.clone(), sender);
        if let Err(error) = self.write_line(&serialize_json_line(&record)).await {
            lock(&self.shared.pending).remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(self.options.request_timeout, receiver).await {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(message))) => Err(anyhow::anyhow!(message)),
            Ok(Err(_)) => Err(anyhow::anyhow!(self.exit_error().unwrap_or_else(|| "Client stopped".into()))),
            Err(_) => {
                lock(&self.shared.pending).remove(&id);
                anyhow::bail!("Timeout waiting for response to {kind}. Stderr: {}", self.shared.stderr())
            }
        }
    }

    /// Send `command` and return its `data` (`null` when absent), or its error.
    pub async fn request(&self, command: RpcCommand) -> anyhow::Result<Value> {
        let mut response = self.send(&command).await?;
        if response.get("success").and_then(Value::as_bool) != Some(true) {
            let message = response.get("error").and_then(Value::as_str).unwrap_or("Unknown error");
            anyhow::bail!(message.to_string());
        }
        Ok(response.get_mut("data").map(Value::take).unwrap_or(Value::Null))
    }
}
