//! The `terminal/*` client methods: commands that the agent runs through pi, with their output
//! kept up to `outputByteLimit` bytes (the oldest bytes go first).

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;

const DEFAULT_LIMIT: usize = 1024 * 1024;

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    truncated: bool,
}

struct Terminal {
    output: Arc<Mutex<Output>>,
    released: bool,
    exit: watch::Receiver<Option<Value>>,
    kill: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Default)]
pub(crate) struct Terminals {
    open: Arc<Mutex<HashMap<String, Terminal>>>,
}

fn append(output: &Mutex<Output>, chunk: &[u8], limit: usize) {
    let mut output = output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    output.bytes.extend_from_slice(chunk);
    if output.bytes.len() > limit {
        let mut cut = output.bytes.len() - limit;
        while cut < output.bytes.len() && (output.bytes[cut] & 0xC0) == 0x80 {
            cut += 1;
        }
        output.bytes.drain(..cut);
        output.truncated = true;
    }
}

fn pump(reader: impl AsyncRead + Unpin + Send + 'static, output: Arc<Mutex<Output>>, limit: usize) {
    tokio::spawn(async move {
        let mut reader = reader;
        let mut buffer = [0u8; 8192];
        while let Ok(read) = reader.read(&mut buffer).await {
            if read == 0 {
                break;
            }
            append(&output, &buffer[..read], limit);
        }
    });
}

fn exit_status(status: std::io::Result<std::process::ExitStatus>) -> Value {
    let status = status.ok();
    #[cfg(unix)]
    let signal = status.and_then(|status| std::os::unix::process::ExitStatusExt::signal(&status));
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    json!({"exitCode": status.and_then(|status| status.code()), "signal": signal.map(|signal| signal.to_string())})
}

async fn supervise(mut child: Child, exit: watch::Sender<Option<Value>>, kill: Arc<tokio::sync::Notify>) {
    let status = tokio::select! {
        status = child.wait() => status,
        () = kill.notified() => {
            let _ = child.start_kill();
            child.wait().await
        }
    };
    let _ = exit.send(Some(exit_status(status)));
}

impl Terminals {
    pub fn create(&self, params: &Value, cwd: &Path) -> Result<Value, String> {
        let command = params["command"].as_str().ok_or("terminal/create needs a command")?;
        let directory = match params["cwd"].as_str() {
            Some(dir) => super::client::inside(cwd, dir)?,
            None => cwd.to_path_buf(),
        };
        let mut process = Command::new(command);
        let args = params["args"].as_array().into_iter().flatten().filter_map(Value::as_str);
        process.args(args).current_dir(directory).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for variable in params["env"].as_array().into_iter().flatten() {
            if let (Some(name), Some(value)) = (variable["name"].as_str(), variable["value"].as_str()) {
                process.env(name, value);
            }
        }
        process.kill_on_drop(true);
        let mut child = process.spawn().map_err(|error| format!("Cannot run {command}: {error}"))?;
        let limit = params["outputByteLimit"].as_u64().map_or(DEFAULT_LIMIT, |limit| limit as usize);
        let output: Arc<Mutex<Output>> = Arc::default();
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, output.clone(), limit);
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, output.clone(), limit);
        }
        let (sender, exit) = watch::channel(None);
        let kill = Arc::new(tokio::sync::Notify::new());
        tokio::spawn(supervise(child, sender, kill.clone()));
        let id = format!("term-{}", crate::auth::oauth::random_uuid());
        let terminal = Terminal { output, released: false, exit, kill };
        self.open.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(id.clone(), terminal);
        Ok(json!({"terminalId": id}))
    }

    fn with<T>(&self, id: &str, read: impl FnOnce(&Terminal) -> T) -> Result<T, String> {
        let open = self.open.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        open.get(id).filter(|terminal| !terminal.released).map(read).ok_or_else(|| format!("Unknown terminal {id}"))
    }

    /// The output so far as `(text, truncated)`, for a finished tool call too.
    pub fn text(&self, id: &str) -> Option<(String, bool)> {
        let open = self.open.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let output = open.get(id)?.output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Some((String::from_utf8_lossy(&output.bytes).into_owned(), output.truncated))
    }

    pub fn output(&self, params: &Value) -> Result<Value, String> {
        let id = params["terminalId"].as_str().unwrap_or_default();
        let exit = self.with(id, |terminal| terminal.exit.borrow().clone())?;
        let (text, truncated) = self.text(id).unwrap_or_default();
        let mut reply = json!({"output": text, "truncated": truncated});
        if let Some(exit) = exit {
            reply["exitStatus"] = exit;
        }
        Ok(reply)
    }

    pub async fn wait_for_exit(&self, params: &Value) -> Result<Value, String> {
        let id = params["terminalId"].as_str().unwrap_or_default();
        let mut exit = self.with(id, |terminal| terminal.exit.clone())?;
        let status = exit.wait_for(Option::is_some).await.map_err(|_| "The terminal closed".to_string())?;
        Ok(status.clone().unwrap_or(Value::Null))
    }

    pub fn kill(&self, params: &Value) -> Result<Value, String> {
        let id = params["terminalId"].as_str().unwrap_or_default();
        self.with(id, |terminal| terminal.kill.notify_one())?;
        Ok(json!({}))
    }

    /// Stops the command; its output stays readable for the transcript.
    pub fn release(&self, params: &Value) -> Result<Value, String> {
        let id = params["terminalId"].as_str().unwrap_or_default();
        let mut open = self.open.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let terminal =
            open.get_mut(id).filter(|terminal| !terminal.released).ok_or_else(|| format!("Unknown terminal {id}"))?;
        terminal.released = true;
        terminal.kill.notify_one();
        Ok(json!({}))
    }
}
