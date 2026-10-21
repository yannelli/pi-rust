//! The RPC loop (upstream `runRpcMode`): read records, route dialog responses, run commands
//! concurrently, stream session events, and shut down on end of input, a closed output, an
//! extension request, or SIGTERM and SIGHUP.

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};

use serde_json::{Value, json};
use tokio::io::AsyncRead;

use super::host::{RpcExtensionBinding, RpcExtensionHost, RpcShutdownHandle};
use super::jsonl::JsonlReader;
use super::output::RpcOutput;
use super::types::{InputRecord, error, parse_input};
use super::ui::RpcExtensionUi;
use crate::agent_session::{AgentSession, AgentSessionEvent, AgentSessionRuntime, lock};

/// Options of [`serve_rpc`].
#[derive(Clone, Default)]
pub struct RpcModeOptions {
    pub extension_host: Option<Arc<dyn RpcExtensionHost>>,
    /// Exit on SIGTERM (143) and SIGHUP (129) on Unix.
    pub handle_signals: bool,
}

/// A command's reply: a record, or a pre-serialized line for deep payloads.
pub(super) enum Reply {
    Json(Value),
    Line(String),
}

pub(super) struct RpcServer {
    pub runtime: Arc<AgentSessionRuntime>,
    pub output: RpcOutput,
    pub ui: Arc<RpcExtensionUi>,
    pub host: Option<Arc<dyn RpcExtensionHost>>,
    pub shutdown: RpcShutdownHandle,
    subscription: Mutex<Option<(Arc<AgentSession>, u64)>>,
    weak: Weak<RpcServer>,
}

type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Run `task` until its first real wait, then let it continue in the background. Like a
/// JavaScript async call, work that does not wait finishes before the next record is read.
pub(super) async fn run_eagerly(mut task: Task) {
    if futures::poll!(task.as_mut()).is_pending() {
        tokio::spawn(task);
    }
}

#[cfg(unix)]
async fn termination_signal() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hangup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => 143,
        _ = hangup.recv() => 129,
    }
}

#[cfg(not(unix))]
async fn termination_signal() -> i32 {
    std::future::pending().await
}

impl RpcServer {
    fn this(&self) -> Arc<Self> {
        self.weak.upgrade().expect("the server outlives its tasks")
    }

    /// Stream the events of `session` and bind extensions to it, in place of the old session.
    fn bind(&self, session: &Arc<AgentSession>) {
        {
            let mut current = lock(&self.subscription);
            if let Some((old, id)) = current.take() {
                old.unsubscribe(id);
                super::builtins::unbind_builtins(&old);
            }
            let output = self.output.clone();
            let shutdown = self.shutdown.clone();
            let id = session.subscribe(move |event| {
                if !event.is_json_event() {
                    return;
                }
                output.send(&event.to_json());
                if matches!(event, AgentSessionEvent::AgentSettled { .. }) {
                    shutdown.check();
                }
            });
            *current = Some((session.clone(), id));
        }
        super::builtins::bind_builtins(session, &self.ui);
        if let Some(host) = &self.host {
            let binding = RpcExtensionBinding::new(
                self.runtime.clone(),
                session.clone(),
                self.ui.clone(),
                self.shutdown.clone(),
                self.output.clone(),
            );
            host.bind(binding);
        }
    }

    async fn reply(&self, reply: Reply) {
        match reply {
            Reply::Json(value) => self.output.send(&value),
            Reply::Line(line) => self.output.send_line(line),
        }
        self.output.drained().await;
    }

    async fn handle_line(self: Arc<Self>, line: String) {
        let (id, kind, command) = match parse_input(&line) {
            Err(message) => {
                self.reply(Reply::Json(error(None, Some(&json!("parse")), &message))).await;
                return;
            }
            Ok(InputRecord::UiResponse { id, response }) => {
                self.ui.resolve(&id, response);
                return;
            }
            Ok(InputRecord::Command { id, kind, command }) => (id, kind, command),
        };
        let reply = match command {
            Err(message) => Some(Reply::Json(error(id.as_ref(), kind.as_ref(), &message))),
            Ok(command) => match self.handle_command(id.as_ref(), command).await {
                Ok(reply) => reply,
                Err(failure) => {
                    let message = format!("{failure:#}");
                    self.reply(Reply::Json(error(id.as_ref(), kind.as_ref(), &message))).await;
                    return;
                }
            },
        };
        if let Some(reply) = reply {
            self.reply(reply).await;
        }
        self.shutdown.check();
    }

    /// Read and dispatch records until the loop must stop; returns the exit code.
    async fn read_loop(&self, input: impl AsyncRead + Unpin, handle_signals: bool) -> i32 {
        let mut reader = JsonlReader::new(input);
        let closed = self.output.closed();
        let signal = async { if handle_signals { termination_signal().await } else { std::future::pending().await } };
        tokio::pin!(signal);
        loop {
            tokio::select! {
                line = reader.next_line() => match line {
                    Ok(Some(line)) => run_eagerly(Box::pin(self.this().handle_line(line))).await,
                    Ok(None) => return 0,
                    Err(failure) => {
                        eprintln!("Error: could not read RPC input: {failure}");
                        return 0;
                    }
                },
                _ = closed.cancelled() => return 1,
                _ = self.shutdown.token().cancelled() => return 0,
                code = &mut signal => return code,
            }
        }
    }

    /// Stop streaming, dispose of the runtime, and flush the output unless killed by SIGTERM.
    async fn finish(&self, code: i32) -> i32 {
        self.runtime.set_rebind_session(None);
        if let Some((session, id)) = lock(&self.subscription).take() {
            session.unsubscribe(id);
            super::builtins::unbind_builtins(&session);
        }
        self.runtime.dispose().await;
        if code != 143 {
            self.output.drained().await;
        }
        code
    }
}

/// Serve the protocol over `input` and `output` until shutdown; returns the exit code.
pub async fn serve_rpc(
    runtime: Arc<AgentSessionRuntime>,
    input: impl AsyncRead + Unpin + Send,
    output: Box<dyn Write + Send>,
    options: RpcModeOptions,
) -> i32 {
    let output = RpcOutput::spawn(output);
    let server = Arc::new_cyclic(|weak| RpcServer {
        runtime: runtime.clone(),
        output: output.clone(),
        ui: RpcExtensionUi::new(output),
        host: options.extension_host.clone(),
        shutdown: RpcShutdownHandle::default(),
        subscription: Mutex::new(None),
        weak: weak.clone(),
    });
    server.bind(&runtime.session());
    let weak = Arc::downgrade(&server);
    runtime.set_rebind_session(Some(Arc::new(move |session| {
        if let Some(server) = weak.upgrade() {
            server.bind(session);
        }
    })));
    let code = server.read_loop(input, options.handle_signals).await;
    server.finish(code).await
}

/// `pi --mode rpc`: take over stdout and serve stdin until it closes.
pub async fn run_rpc_mode(runtime: Arc<AgentSessionRuntime>, options: RpcModeOptions) -> i32 {
    if let Err(failure) = crate::diagnostics::take_over_stdout() {
        eprintln!("Error: could not reserve stdout for RPC mode: {failure}");
        return 1;
    }
    let options = RpcModeOptions { handle_signals: true, ..options };
    serve_rpc(runtime, tokio::io::stdin(), Box::new(super::output::RawStdout), options).await
}
