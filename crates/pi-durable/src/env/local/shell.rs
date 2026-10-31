//! The `Shell` implementation of `LocalExecutionEnv`: spawning, streaming, spills, timeouts, and process-tree kills.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_chord::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::LocalExecutionEnv;
use super::process::{exit_code, kill_process_tree, shell_config};
use super::spill::Spill;
use crate::env::decode::StreamDecoder;
use crate::env::{
    ExecutionError, ExecutionErrorCode, OutputStream, Shell, ShellCommand, ShellExecOptions, ShellExecResult,
    ShellOutputInfo,
};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// After the shell exits, how long output may pause before descendants holding stdio open are no longer waited for.
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);
const READ_CHUNK: usize = 64 * 1024;

fn resolve_timeout(timeout: Option<f64>) -> Result<Option<Duration>, ExecutionError> {
    let Some(seconds) = timeout else { return Ok(None) };
    if !seconds.is_finite() || seconds <= 0.0 {
        let message = "Invalid timeout: must be a finite number of seconds";
        return Err(ExecutionError::new(ExecutionErrorCode::Timeout, message));
    }
    if seconds * 1000.0 > MAX_TIMEOUT_MS {
        let message = format!("Invalid timeout: maximum is {} seconds", MAX_TIMEOUT_MS / 1000.0);
        return Err(ExecutionError::new(ExecutionErrorCode::Timeout, message));
    }
    Ok(Some(Duration::from_secs_f64(seconds)))
}

type Chunks = mpsc::Sender<(OutputStream, Vec<u8>)>;

/// Read one pipe until it ends or `stop` is set. Every chunk already read is sent; after `stop`, on Unix, also what
/// the pipe can give without waiting, up to one chunk.
fn spawn_reader(
    mut stream: impl AsyncRead + Unpin + Send + 'static,
    source: OutputStream,
    sender: Chunks,
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; READ_CHUNK];
        while !*stop.borrow() {
            let count = tokio::select! {
                biased;
                read = stream.read(&mut buffer) => match read {
                    Ok(count) => count,
                    Err(_) => return,
                },
                _ = stop.wait_for(|stopped| *stopped) => break,
            };
            if count == 0 || sender.send((source, buffer[..count].to_vec())).await.is_err() {
                return;
            }
        }
        drain(&mut stream, &mut buffer, source, &sender).await;
    })
}

/// Send the bytes a Unix pipe holds now, up to one chunk, without waiting for more or for its end.
#[cfg(unix)]
async fn drain(stream: &mut (impl AsyncRead + Unpin), buffer: &mut [u8], source: OutputStream, sender: &Chunks) {
    use futures::FutureExt;
    let mut drained = 0;
    while drained < buffer.len() {
        let limit = buffer.len() - drained;
        let Some(Ok(count)) = stream.read(&mut buffer[..limit]).now_or_never() else { return };
        if count == 0 || sender.send((source, buffer[..count].to_vec())).await.is_err() {
            return;
        }
        drained += count;
    }
}

/// Windows pipes read on a blocking thread, so a read started now would not return at once; nothing more is read.
#[cfg(not(unix))]
async fn drain(_stream: &mut (impl AsyncRead + Unpin), _buffer: &mut [u8], _source: OutputStream, _sender: &Chunks) {}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast_ref::<&str>().map(|message| message.to_string()).unwrap_or_default(),
    }
}

/// Kills the command's process tree unless it settled, and forgets its pid.
struct ChildGuard {
    pid: Option<u32>,
    active: Arc<Mutex<HashSet<u32>>>,
    settled: bool,
}

impl ChildGuard {
    fn kill(&self) {
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.kill();
        }
        if let Some(pid) = self.pid {
            self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&pid);
        }
    }
}

struct Output<'a> {
    options: &'a ShellExecOptions,
    context: &'a Context,
    stdout: StreamDecoder,
    stderr: StreamDecoder,
    callback_error: Option<ExecutionError>,
}

impl Output<'_> {
    /// Deliver decoded text; returns false when the callback failed just now.
    fn emit(&mut self, text: &str, stream: OutputStream) -> bool {
        let Some(on_output) = self.options.on_output.as_ref() else { return true };
        if text.is_empty() || self.callback_error.is_some() {
            return true;
        }
        let info = ShellOutputInfo { stream, skipped: None };
        match std::panic::catch_unwind(AssertUnwindSafe(|| on_output(text, self.context, &info))) {
            Ok(()) => true,
            Err(payload) => {
                let message = panic_message(payload);
                self.callback_error = Some(ExecutionError::new(ExecutionErrorCode::CallbackError, message));
                false
            }
        }
    }

    fn decode(&mut self, stream: OutputStream, bytes: &[u8]) -> bool {
        let text = match stream {
            OutputStream::Stdout => self.stdout.decode(bytes),
            OutputStream::Stderr => self.stderr.decode(bytes),
        };
        self.emit(&text, stream)
    }
}

/// Decode and spill one chunk, killing the command at once when the callback or the spill fails.
async fn take_chunk(
    output: &mut Output<'_>,
    spill: &mut Option<Spill>,
    guard: &ChildGuard,
    env: &LocalExecutionEnv,
    stream: OutputStream,
    bytes: &[u8],
) {
    if !output.decode(stream, bytes) {
        guard.kill();
    }
    if let Some(spill) = spill.as_mut() {
        if !spill.feed(bytes, env, output.context).await {
            guard.kill();
        }
    }
}

impl LocalExecutionEnv {
    fn command(&self, command: &ShellCommand) -> Result<(tokio::process::Command, Option<String>), ExecutionError> {
        // A string runs through the shell; an argv array runs its program directly, without shell parsing.
        match command {
            ShellCommand::Script(script) => {
                let config = shell_config(self.shell_path.as_deref())?;
                let mut process = tokio::process::Command::new(&config.shell);
                process.args(&config.args);
                if config.command_from_stdin {
                    return Ok((process, Some(script.clone())));
                }
                process.arg(script);
                Ok((process, None))
            }
            ShellCommand::Argv(argv) => {
                let Some((program, args)) = argv.split_first() else {
                    let message = "Empty argv: no program to run";
                    return Err(ExecutionError::new(ExecutionErrorCode::SpawnError, message));
                };
                let mut process = tokio::process::Command::new(program);
                process.args(args);
                Ok((process, None))
            }
        }
    }
}

#[async_trait]
impl Shell for LocalExecutionEnv {
    async fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        if context.is_cancelled() {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        let timeout = resolve_timeout(options.timeout)?;
        let cwd = match options.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => self.resolve(cwd),
            _ => self.current_cwd(),
        };
        let (mut process, stdin_command) = self.command(&command)?;
        if let Err(error) = std::fs::metadata(&cwd) {
            let message = format!("Working directory does not exist: {cwd}\nCannot execute bash commands.");
            return Err(ExecutionError::new(ExecutionErrorCode::SpawnError, message).with_cause(Arc::new(error)));
        }

        if options.inherit_env.unwrap_or(true) {
            process.envs(self.shell_env.iter().flatten());
        } else {
            process.env_clear();
        }
        process.envs(options.env.iter().flatten());
        process
            .current_dir(Path::new(&cwd))
            .stdin(if stdin_command.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        process.process_group(0);
        #[cfg(windows)]
        process.creation_flags(super::process::CREATE_NO_WINDOW);
        let mut child = process.spawn().map_err(|error| {
            ExecutionError::new(ExecutionErrorCode::SpawnError, error.to_string()).with_cause(Arc::new(error))
        })?;
        let mut guard = ChildGuard { pid: child.id(), active: self.active_pids.clone(), settled: false };
        if let Some(pid) = guard.pid {
            self.active_pids.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(pid);
        }
        if let (Some(mut stdin), Some(script)) = (child.stdin.take(), stdin_command) {
            tokio::spawn(async move {
                let _ = stdin.write_all(script.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }

        let (sender, mut receiver) = mpsc::channel(8);
        let (stop, stopped) = watch::channel(false);
        let readers = [
            child
                .stdout
                .take()
                .map(|stdout| spawn_reader(stdout, OutputStream::Stdout, sender.clone(), stopped.clone())),
            child.stderr.take().map(|stderr| spawn_reader(stderr, OutputStream::Stderr, sender, stopped)),
        ];
        let signal = context.abort_signal();
        let abort = async {
            match &signal {
                Some(signal) => drop(signal.cancelled().await),
                None => std::future::pending().await,
            }
        };
        tokio::pin!(abort);
        let far = Instant::now() + Duration::from_secs(86_400 * 365);
        let deadline = timeout.map_or(far, |timeout| Instant::now() + timeout);
        let mut output = Output {
            options: &options,
            context,
            stdout: StreamDecoder::new(),
            stderr: StreamDecoder::new(),
            callback_error: None,
        };
        let mut spill = options.spill.map(Spill::new);
        let (mut aborted, mut timed_out, mut streams_done) = (false, false, false);
        let (mut status, mut idle) = (None, far);
        let mut wait_error = None;

        while status.is_none() || !streams_done {
            tokio::select! {
                () = &mut abort, if !aborted => {
                    aborted = true;
                    guard.kill();
                }
                () = tokio::time::sleep_until(deadline), if !timed_out && timeout.is_some() => {
                    timed_out = true;
                    guard.kill();
                }
                chunk = receiver.recv(), if !streams_done => match chunk {
                    Some((stream, bytes)) => {
                        take_chunk(&mut output, &mut spill, &guard, self, stream, &bytes).await;
                        if status.is_some() {
                            idle = Instant::now() + EXIT_STDIO_GRACE;
                        }
                    }
                    None => streams_done = true,
                },
                result = child.wait(), if status.is_none() => {
                    match result {
                        Ok(exit) => status = Some(exit_code(exit)),
                        Err(error) => {
                            wait_error = Some(error);
                            break;
                        }
                    }
                    idle = Instant::now() + EXIT_STDIO_GRACE;
                }
                () = tokio::time::sleep_until(idle), if status.is_some() => break,
            }
        }
        // The readers end promptly once stopped; what they read before, or can read at once, is still delivered.
        let _ = stop.send(true);
        while let Some((stream, bytes)) = receiver.recv().await {
            take_chunk(&mut output, &mut spill, &guard, self, stream, &bytes).await;
        }
        for reader in readers.into_iter().flatten() {
            let _ = reader.await;
        }
        if let Some(spill) = spill.as_mut() {
            spill.finish().await;
        }
        let stdout_tail = output.stdout.finish();
        output.emit(&stdout_tail, OutputStream::Stdout);
        let stderr_tail = output.stderr.finish();
        output.emit(&stderr_tail, OutputStream::Stderr);
        guard.settled = true;
        drop(guard);

        if let Some(error) = wait_error {
            let message = error.to_string();
            return Err(ExecutionError::new(ExecutionErrorCode::SpawnError, message).with_cause(Arc::new(error)));
        }
        if let Some(error) = output.callback_error {
            return Err(error);
        }
        let spill_path = spill.as_ref().and_then(|spill| spill.path.clone());
        let interrupted = if timed_out {
            let seconds = options.timeout.unwrap_or_default();
            Some(ExecutionError::new(ExecutionErrorCode::Timeout, format!("timeout:{seconds}")))
        } else if aborted || context.is_cancelled() {
            Some(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"))
        } else {
            None
        };
        if let Some(mut interrupted) = interrupted {
            interrupted.spill_path = spill_path;
            return Err(interrupted);
        }
        if let Some(error) = spill.and_then(|spill| spill.error) {
            return Err(error);
        }
        Ok(ShellExecResult { exit_code: status.unwrap_or(1), spill_path })
    }
}

#[cfg(test)]
mod reader_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_timeouts() {
        assert!(resolve_timeout(Some(0.0)).is_err());
        assert!(resolve_timeout(Some(f64::NAN)).is_err());
        assert_eq!(
            resolve_timeout(Some(2_147_484.0)).unwrap_err().message,
            "Invalid timeout: maximum is 2147483.647 seconds"
        );
        assert_eq!(resolve_timeout(Some(1.5)).unwrap(), Some(Duration::from_millis(1500)));
    }
}
