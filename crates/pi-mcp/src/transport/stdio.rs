//! Stdio transport: newline-delimited JSON-RPC with a server process in its own process group.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::watch;

use super::process::{self, StopSignal};
use super::{DEFAULT_MAX_MESSAGE_BYTES, McpTransport, TransportSink};
use crate::error::McpError;
use crate::jsonrpc::JsonRpcMessage;

const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a server gets to exit after stdin closes, before SIGTERM.
const STDIN_CLOSE_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StderrMode {
    #[default]
    Pipe,
    Inherit,
}

pub type StderrCallback = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct StdioTransportOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// Start from the host's environment. Default: true.
    pub inherit_env: bool,
    pub stderr: StderrMode,
    pub on_stderr: Option<StderrCallback>,
    pub max_message_bytes: usize,
    pub max_stderr_bytes: usize,
    /// Time to wait after SIGTERM before SIGKILL. Default: 2 s.
    pub close_timeout: Duration,
}

impl StdioTransportOptions {
    pub fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
            cwd: None,
            env: BTreeMap::new(),
            inherit_env: true,
            stderr: StderrMode::Pipe,
            on_stderr: None,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
        }
    }
}

#[derive(Default)]
struct State {
    started: bool,
    closed: bool,
    pid: Option<u32>,
    sink: Option<TransportSink>,
}

pub struct StdioTransport {
    options: StdioTransportOptions,
    state: Mutex<State>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    exited: watch::Sender<bool>,
}

impl StdioTransport {
    pub fn new(options: StdioTransportOptions) -> Self {
        Self {
            options,
            state: Mutex::default(),
            stdin: tokio::sync::Mutex::new(None),
            stderr: Arc::default(),
            exited: watch::channel(false).0,
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.lock().pid
    }

    /// The last stderr output, up to `max_stderr_bytes`.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap_or_else(|poisoned| poisoned.into_inner())).into_owned()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn has_exited(&self) -> bool {
        *self.exited.borrow()
    }

    async fn wait_exit(&self) {
        let mut receiver = self.exited.subscribe();
        let _ = receiver.wait_for(|exited| *exited).await;
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn start(&self, sink: TransportSink) -> Result<(), McpError> {
        {
            let mut state = self.lock();
            if state.started {
                return Err(McpError::other("MCP stdio transport already started"));
            }
            if state.closed {
                return Err(McpError::closed());
            }
            state.started = true;
            state.sink = Some(sink.clone());
        }
        let options = &self.options;
        let env_path = options.env.get("PATH").map(String::as_str);
        let mut command = Command::new(process::resolve_command(&options.command, env_path));
        command.args(&options.args).stdin(Stdio::piped()).stdout(Stdio::piped());
        command.stderr(if options.stderr == StderrMode::Inherit { Stdio::inherit() } else { Stdio::piped() });
        if options.inherit_env {
            command.envs(pi_ai::env::child_process_env());
        } else {
            command.env_clear();
        }
        command.envs(&options.env);
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        // Own process group, so closing can stop the server's children too.
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let mut child =
            command.spawn().map_err(|error| McpError::other(process::spawn_error(&options.command, &error)))?;
        let pid = child.id();
        if let Some(pid) = pid {
            process::track(pid);
        }
        self.lock().pid = pid;
        *self.stdin.lock().await = child.stdin.take();

        let stdout = child
            .stdout
            .take()
            .map(|stdout| tokio::spawn(read_stdout(stdout, sink.clone(), options.max_message_bytes)));
        if let Some(stderr) = child.stderr.take() {
            let buffer = self.stderr.clone();
            tokio::spawn(read_stderr(stderr, buffer, options.max_stderr_bytes, options.on_stderr.clone()));
        }
        let exited = self.exited.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            if let Some(pid) = pid {
                process::untrack(pid);
            }
            exited.send_replace(true);
            if let Some(stdout) = stdout {
                let _ = stdout.await;
            }
            sink.close();
        });
        Ok(())
    }

    async fn send(&self, message: &JsonRpcMessage) -> Result<(), McpError> {
        {
            let state = self.lock();
            if !state.started || state.closed {
                return Err(McpError::closed());
            }
        }
        let mut stdin = self.stdin.lock().await;
        let Some(writer) = stdin.as_mut() else { return Err(McpError::closed()) };
        let mut payload = message.to_json();
        payload.push('\n');
        let result = async {
            writer.write_all(payload.as_bytes()).await?;
            writer.flush().await
        }
        .await;
        result.map_err(|error| McpError::ConnectionClosed(format!("MCP stdio write failed: {error}")))
    }

    async fn close(&self) {
        let (pid, sink) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            (state.pid, state.sink.clone())
        };
        // Shutdown per the spec: close stdin and let the server exit, then SIGTERM, then SIGKILL.
        self.stdin.lock().await.take();
        let Some(pid) = pid else {
            if let Some(sink) = sink {
                sink.close();
            }
            return;
        };
        if self.has_exited() {
            return;
        }
        let close_timeout = self.options.close_timeout;
        let grace = STDIN_CLOSE_GRACE.min(close_timeout);
        if tokio::time::timeout(grace, self.wait_exit()).await.is_err() {
            process::kill_tree(pid, StopSignal::Term);
            if tokio::time::timeout(close_timeout, self.wait_exit()).await.is_err() {
                process::kill_tree(pid, StopSignal::Kill);
                self.wait_exit().await;
            }
        }
        // Children of the server that ignored stdin closing would otherwise outlive it.
        process::kill_tree(pid, StopSignal::Term);
        if let Some(sink) = sink {
            sink.close();
        }
    }

    fn stderr(&self) -> Option<String> {
        Some(self.stderr_text())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let state = self.lock();
        if let (Some(pid), false) = (state.pid, state.closed || *self.exited.borrow()) {
            process::kill_tree(pid, StopSignal::Term);
        }
    }
}

async fn read_stdout(mut stdout: impl AsyncRead + Unpin, sink: TransportSink, max_message_bytes: usize) {
    let too_large = || McpError::other(format!("MCP stdio message exceeds {max_message_bytes} bytes"));
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                sink.error(McpError::other(error.to_string()));
                break;
            }
        };
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=newline).collect();
            let line = &line[..line.len() - 1];
            if line.len() > max_message_bytes {
                sink.error(too_large());
                continue;
            }
            let text = String::from_utf8_lossy(line);
            let text = text.strip_suffix('\r').unwrap_or(&text);
            if text.trim().is_empty() {
                continue;
            }
            match JsonRpcMessage::from_json(text) {
                Ok(message) => sink.message(message),
                Err(error) => sink.error(error),
            }
        }
        if buffer.len() > max_message_bytes {
            buffer.clear();
            sink.error(too_large());
        }
    }
    if !String::from_utf8_lossy(&buffer).trim().is_empty() {
        sink.error(McpError::other("MCP stdio server closed with an incomplete JSON-RPC message"));
    }
}

async fn read_stderr(
    mut stderr: impl AsyncRead + Unpin,
    buffer: Arc<Mutex<Vec<u8>>>,
    max_bytes: usize,
    on_stderr: Option<StderrCallback>,
) {
    let mut chunk = vec![0u8; 16 * 1024];
    while let Ok(read) = stderr.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        {
            let mut buffer = buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > max_bytes {
                let excess = buffer.len() - max_bytes;
                buffer.drain(..excess);
            }
        }
        if let Some(callback) = &on_stderr {
            callback(&String::from_utf8_lossy(&chunk[..read]));
        }
    }
}
