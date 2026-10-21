//! Upstream `forkDriver`: spawns the child host process with a control channel, mirrors its output and kills its
//! process tree when it exits.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use super::control::{ControlMessage, IPC_ENV, LOCK_ENV};
use super::{ControlListener, Driver, Exit, Instance, os};
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::ChordErrorCode;
use crate::host2::link::{IpcChannel, IpcLink, Link, StreamChannel, ipc_link};
use crate::json::Json;
use crate::sync::{BoxFuture, lock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

pub type OutputTap = Arc<dyn Fn(OutputStream, &[u8]) + Send + Sync>;
pub type MessageTap = Arc<dyn Fn(&Json) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ForkDriverOptions {
    /// The child's executable (upstream: its entry module).
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Added to this process's environment.
    pub env: Vec<(OsString, OsString)>,
    /// Sent to the child as its first control message ([`super::ChildControl::init`]).
    pub init: Option<Json>,
    /// A lock name: one live process per name, across managers and processes. The child acquires it as a kernel
    /// lock ([`super::ChildControl::lock`], before opening its storage); a second child fails with `unavailable/locked`.
    pub lock: Option<String>,
    /// Optional stdout/stderr tap. The streams are still mirrored to this process's.
    pub on_output: Option<OutputTap>,
    /// Optional tap of every channel message; control messages and chord frames keep their normal handling.
    pub on_message: Option<MessageTap>,
}

impl ForkDriverOptions {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), ..Self::default() }
    }
}

pub struct ForkDriver {
    options: ForkDriverOptions,
}

impl ForkDriver {
    pub fn new(options: ForkDriverOptions) -> Self {
        Self { options }
    }
}

impl Driver for ForkDriver {
    /// Must be called within a Tokio runtime.
    fn spawn(&self) -> ChordResult<Arc<dyn Instance>> {
        let options = &self.options;
        let mut command = Command::new(&options.program);
        // A lock this process inherited (as a child host itself) is not the child's: it gets only its own.
        command.args(&options.args).env_remove(LOCK_ENV).env_remove(IPC_ENV);
        command.envs(options.env.iter().map(|(key, value)| (key, value)));
        if let Some(name) = &options.lock {
            command.env(LOCK_ENV, name);
        }
        let output = || if options.on_output.is_some() { Stdio::piped() } else { Stdio::inherit() };
        command.stdout(output()).stderr(output());
        os::detach(&mut command);
        let (exit_tx, exit_rx) = watch::channel(None);
        let failed = |error: std::io::Error| {
            let message = format!("cannot start {}: {error}", options.program.display());
            ChordError::code_only(ChordErrorCode::Unavailable, message)
        };
        let channel = os::parent_channel(&mut command, exit_rx.clone()).map_err(failed)?;
        // Every listener before the child runs: none of its messages may reach nobody.
        let listeners: Arc<Mutex<Vec<ControlListener>>> = Arc::default();
        let dispatch = listeners.clone();
        let tap = options.on_message.clone();
        channel.on_message(Arc::new(move |message| {
            if let Some(tap) = &tap {
                tap(message);
            }
            if let Some(control) = ControlMessage::unwrap(message) {
                let listeners = lock(&dispatch).clone();
                for listener in listeners {
                    listener(&control);
                }
            }
        }));
        let link = ipc_link(channel.clone());
        let spawned = command.spawn();
        // The command holds this process's copy of the child's end of the channel.
        drop(command);
        let mut child = spawned.map_err(failed)?;
        let pid = child.id().unwrap_or(0);
        if let Some(tap) = &options.on_output {
            if let Some(stdout) = child.stdout.take() {
                mirror(stdout, tokio::io::stdout(), OutputStream::Stdout, tap.clone());
            }
            if let Some(stderr) = child.stderr.take() {
                mirror(stderr, tokio::io::stderr(), OutputStream::Stderr, tap.clone());
            }
        }
        let (kill, kills) = mpsc::unbounded_channel();
        tokio::spawn(wait_exit(child, pid, kills, exit_tx));
        let instance = ForkInstance { pid, link, channel, exit: exit_rx, kill, listeners };
        if let Some(value) = &options.init {
            instance.send(&ControlMessage::Init { value: value.clone() });
        }
        Ok(Arc::new(instance))
    }
}

/// Waits for the exit; a kill request in the meantime kills the child itself.
async fn wait_exit(
    mut child: Child,
    pid: u32,
    mut kills: mpsc::UnboundedReceiver<()>,
    exit: watch::Sender<Option<Exit>>,
) {
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            Some(()) = kills.recv() => {
                let _ = child.start_kill();
            }
        }
    };
    // However the leader ended (graceful, crash, kill), nothing it spawned outlives it.
    os::kill_group(pid);
    exit.send_replace(Some(status.map(os::exit_of).unwrap_or_default()));
}

/// The exit `exit` carries, once it does.
pub(super) async fn exit_value(mut exit: watch::Receiver<Option<Exit>>) -> Exit {
    loop {
        if let Some(value) = exit.borrow_and_update().clone() {
            return value;
        }
        if exit.changed().await.is_err() {
            return exit.borrow().clone().unwrap_or_default();
        }
    }
}

fn mirror<R, W>(mut reader: R, mut writer: W, stream: OutputStream, tap: OutputTap)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0; 8192];
        while let Ok(read) = reader.read(&mut buffer).await {
            if read == 0 {
                return;
            }
            tap(stream, &buffer[..read]);
            let _ = writer.write_all(&buffer[..read]).await;
            let _ = writer.flush().await;
        }
    });
}

struct ForkInstance {
    pid: u32,
    link: Arc<IpcLink>,
    channel: Arc<StreamChannel>,
    exit: watch::Receiver<Option<Exit>>,
    kill: mpsc::UnboundedSender<()>,
    listeners: Arc<Mutex<Vec<ControlListener>>>,
}

impl Instance for ForkInstance {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn link(&self) -> Arc<dyn Link> {
        self.link.clone()
    }

    fn exited(&self) -> BoxFuture<Exit> {
        Box::pin(exit_value(self.exit.clone()))
    }

    /// A child that exited before the channel reported it fails the write: its exit is what counts.
    fn send(&self, message: &ControlMessage) {
        if self.channel.connected() {
            self.channel.send(message.wrap(), Some(Box::new(|_| {})));
        }
    }

    fn kill_tree(&self) {
        if !os::kill_group(self.pid) {
            let _ = self.kill.send(());
        }
    }

    fn on_control(&self, listener: ControlListener) {
        lock(&self.listeners).push(listener);
    }
}
